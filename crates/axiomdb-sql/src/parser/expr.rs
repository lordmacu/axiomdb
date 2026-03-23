//! Expression sub-parser — parses [`Expr`] from the token stream.
//!
//! ## Operator precedence (lowest to highest)
//!
//! ```text
//! expr           ::= or_expr
//! or_expr        ::= and_expr (OR and_expr)*
//! and_expr       ::= not_expr (AND not_expr)*
//! not_expr       ::= NOT not_expr | is_null_expr
//! is_null_expr   ::= predicate (IS [NOT] NULL)?
//! predicate      ::= addition ([NOT] BETWEEN addition AND addition)
//!                  | addition [NOT] LIKE atom [ESCAPE atom]
//!                  | addition [NOT] IN '(' expr_list ')'
//!                  | addition (cmp_op addition)?
//! addition       ::= multiplication (('+' | '-' | '||') multiplication)*
//! multiplication ::= unary (('*' | '/' | '%') unary)*
//! unary          ::= '-' unary | atom
//! atom           ::= literal | col_ref | fn_call | '(' expr ')'
//! col_ref        ::= identifier ['.' identifier]
//! fn_call        ::= identifier '(' ([*] | [expr (',' expr)*]) ')'
//! ```
//!
//! Phase 4.3 covered: literals, NOT, comparisons, AND, OR.
//! Phase 4.4 adds: IS NULL, BETWEEN, LIKE, IN, arithmetic, table.col, function calls.

use axiomdb_core::error::DbError;
use axiomdb_types::{DataType, Value};

use crate::{
    expr::{BinaryOp, Expr, UnaryOp},
    lexer::Token,
};

// ── Subquery helper ───────────────────────────────────────────────────────────

/// Parse a full SELECT statement in a subquery context.
///
/// Expects `SELECT` to be the current token; consumes it and parses
/// everything up to (but not including) the closing `)`.
fn parse_subquery(p: &mut super::Parser) -> Result<crate::ast::SelectStmt, DbError> {
    p.expect(&Token::Select)?;
    super::dml::parse_select(p)
}

use super::Parser;

/// Parse a full SQL expression.
pub(crate) fn parse_expr(p: &mut Parser) -> Result<Expr, DbError> {
    parse_or(p)
}

// ── OR ────────────────────────────────────────────────────────────────────────

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

// ── AND ───────────────────────────────────────────────────────────────────────

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

// ── NOT ───────────────────────────────────────────────────────────────────────

fn parse_not(p: &mut Parser) -> Result<Expr, DbError> {
    if p.eat(&Token::Not) {
        // NOT EXISTS (SELECT ...) — handled here before the generic NOT path.
        if matches!(p.peek(), Token::Exists) {
            p.advance();
            p.expect(&Token::LParen)?;
            let query = parse_subquery(p)?;
            p.expect(&Token::RParen)?;
            return Ok(Expr::Exists {
                query: Box::new(query),
                negated: true,
            });
        }
        let operand = parse_not(p)?;
        return Ok(Expr::UnaryOp {
            op: UnaryOp::Not,
            operand: Box::new(operand),
        });
    }
    // EXISTS (SELECT ...) — without NOT.
    if matches!(p.peek(), Token::Exists) {
        p.advance();
        p.expect(&Token::LParen)?;
        let query = parse_subquery(p)?;
        p.expect(&Token::RParen)?;
        return Ok(Expr::Exists {
            query: Box::new(query),
            negated: false,
        });
    }
    parse_is_null(p)
}

// ── IS NULL ───────────────────────────────────────────────────────────────────

fn parse_is_null(p: &mut Parser) -> Result<Expr, DbError> {
    let expr = parse_predicate(p)?;
    if p.eat(&Token::Is) {
        let negated = p.eat(&Token::Not);
        p.expect(&Token::Null)?;
        return Ok(Expr::IsNull {
            expr: Box::new(expr),
            negated,
        });
    }
    Ok(expr)
}

// ── Predicate: BETWEEN, LIKE, IN, comparison ──────────────────────────────────

fn parse_predicate(p: &mut Parser) -> Result<Expr, DbError> {
    let left = parse_addition(p)?;

    // Check for optional NOT before BETWEEN/LIKE/IN.
    // Note: bare NOT here means `a NOT BETWEEN …`, not `NOT a`.
    // Bare NOT before a comparison (`a NOT > b`) is a parse error.
    let negated = if matches!(
        (p.peek(), p.peek_at(1)),
        (Token::Not, Token::Between) | (Token::Not, Token::Like) | (Token::Not, Token::In)
    ) {
        p.advance(); // consume NOT
        true
    } else {
        false
    };

    match p.peek() {
        Token::Between => {
            p.advance();
            let low = parse_addition(p)?;
            p.expect(&Token::And)?;
            let high = parse_addition(p)?;
            Ok(Expr::Between {
                expr: Box::new(left),
                low: Box::new(low),
                high: Box::new(high),
                negated,
            })
        }
        Token::Like => {
            p.advance();
            let pattern = parse_atom(p)?;
            // Optional ESCAPE clause — consume and discard (Phase 4.x)
            if p.eat(&Token::Escape) {
                parse_atom(p)?; // discard escape char
            }
            Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                negated,
            })
        }
        Token::In => {
            p.advance();
            p.expect(&Token::LParen)?;
            // IN (SELECT ...) — subquery membership test.
            if matches!(p.peek(), Token::Select) {
                let query = parse_subquery(p)?;
                p.expect(&Token::RParen)?;
                return Ok(Expr::InSubquery {
                    expr: Box::new(left),
                    query: Box::new(query),
                    negated,
                });
            }
            // IN (value_list) — existing behavior.
            let mut list = vec![parse_expr(p)?];
            while p.eat(&Token::Comma) {
                list.push(parse_expr(p)?);
            }
            p.expect(&Token::RParen)?;
            Ok(Expr::In {
                expr: Box::new(left),
                list,
                negated,
            })
        }
        cmp if !negated => {
            let op = match cmp {
                Token::Eq => BinaryOp::Eq,
                Token::NotEq => BinaryOp::NotEq,
                Token::Lt => BinaryOp::Lt,
                Token::LtEq => BinaryOp::LtEq,
                Token::Gt => BinaryOp::Gt,
                Token::GtEq => BinaryOp::GtEq,
                _ => return Ok(left),
            };
            p.advance();
            let right = parse_addition(p)?;
            Ok(Expr::BinaryOp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            })
        }
        _ if negated => {
            // NOT was consumed but no BETWEEN/LIKE/IN followed — error.
            Err(DbError::ParseError {
                message: format!(
                    "expected BETWEEN, LIKE, or IN after NOT at position {}",
                    p.current_pos()
                ),
            })
        }
        _ => Ok(left),
    }
}

// ── Addition: +, -, || ────────────────────────────────────────────────────────

fn parse_addition(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_multiplication(p)?;
    loop {
        let op = match p.peek() {
            Token::Plus => BinaryOp::Add,
            Token::Minus => BinaryOp::Sub,
            Token::Concat => BinaryOp::Concat,
            _ => break,
        };
        p.advance();
        let right = parse_multiplication(p)?;
        left = Expr::BinaryOp {
            op,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

// ── Multiplication: *, /, % ───────────────────────────────────────────────────

fn parse_multiplication(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_unary(p)?;
    loop {
        let op = match p.peek() {
            Token::Star => BinaryOp::Mul,
            Token::Slash => BinaryOp::Div,
            Token::Percent => BinaryOp::Mod,
            _ => break,
        };
        p.advance();
        let right = parse_unary(p)?;
        left = Expr::BinaryOp {
            op,
            left: Box::new(left),
            right: Box::new(right),
        };
    }
    Ok(left)
}

// ── Unary: unary minus ────────────────────────────────────────────────────────

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

// ── Atom ──────────────────────────────────────────────────────────────────────

fn parse_atom(p: &mut Parser) -> Result<Expr, DbError> {
    let pos = p.current_pos();

    match p.peek().clone() {
        Token::Integer(n) => {
            p.advance();
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
        Token::LParen => {
            p.advance();
            // (SELECT ...) — scalar subquery.
            if matches!(p.peek(), Token::Select) {
                let query = parse_subquery(p)?;
                p.expect(&Token::RParen)?;
                return Ok(Expr::Subquery(Box::new(query)));
            }
            let expr = parse_expr(p)?;
            p.expect(&Token::RParen)?;
            Ok(expr)
        }
        // Identifiers and unreserved keywords usable as column/function names.
        Token::Ident(_)
        | Token::QuotedIdent(_)
        | Token::DqIdent(_)
        | Token::Key
        | Token::Index
        | Token::Tables
        | Token::Desc
        | Token::Action
        | Token::Names
        | Token::Autocommit => parse_ident_or_call(p),

        // ── CASE WHEN ... END ─────────────────────────────────────────────────
        Token::Case => {
            p.advance();

            // Simple CASE if the next token is not WHEN (has a base expression).
            let operand = if !matches!(p.peek(), Token::When) {
                Some(Box::new(parse_expr(p)?))
            } else {
                None
            };

            // Parse one or more WHEN condition/value THEN result pairs.
            let mut when_thens: Vec<(Expr, Expr)> = Vec::new();
            while p.eat(&Token::When) {
                let condition = parse_expr(p)?;
                p.expect(&Token::Then)?;
                let result = parse_expr(p)?;
                when_thens.push((condition, result));
            }
            if when_thens.is_empty() {
                return Err(DbError::ParseError {
                    message: format!(
                        "CASE requires at least one WHEN branch, found {:?} at position {}",
                        p.peek(),
                        p.current_pos()
                    ),
                });
            }

            // Optional ELSE clause.
            let else_result = if p.eat(&Token::Else) {
                Some(Box::new(parse_expr(p)?))
            } else {
                None
            };

            p.expect(&Token::End)?;

            Ok(Expr::Case {
                operand,
                when_thens,
                else_result,
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

/// Parse an identifier (possibly `table.col`) or a function call.
fn parse_ident_or_call(p: &mut Parser) -> Result<Expr, DbError> {
    let name = p.parse_identifier()?;

    // Check for table.column: `name.field`
    if p.eat(&Token::Dot) {
        let field = p.parse_identifier()?;
        let qualified = format!("{name}.{field}");
        // No function call after table.col in Phase 4.4
        return Ok(Expr::Column {
            col_idx: 0,
            name: qualified,
        });
    }

    // Check for function call: `name(`
    if p.eat(&Token::LParen) {
        // CAST(expr AS type) — special syntax, not a regular function call.
        if name.eq_ignore_ascii_case("cast") {
            let expr = parse_expr(p)?;
            if !p.eat(&Token::As) {
                return Err(DbError::ParseError {
                    message: format!("expected AS in CAST at position {}", p.current_pos()),
                });
            }
            let type_name = p.parse_identifier()?;
            p.expect(&Token::RParen)?;
            let target = parse_data_type(&type_name)?;
            return Ok(Expr::Cast {
                expr: Box::new(expr),
                target,
            });
        }

        // COUNT(*) and similar aggregate wildcards
        if p.eat(&Token::Star) {
            p.expect(&Token::RParen)?;
            return Ok(Expr::Function {
                name: name.to_ascii_lowercase(),
                args: vec![],
            });
        }
        // Regular args or no args
        let mut args = Vec::new();
        if !matches!(p.peek(), Token::RParen) {
            args.push(parse_expr(p)?);
            while p.eat(&Token::Comma) {
                args.push(parse_expr(p)?);
            }
        }
        p.expect(&Token::RParen)?;
        return Ok(Expr::Function {
            name: name.to_ascii_lowercase(),
            args,
        });
    }

    // Plain column reference
    Ok(Expr::Column { col_idx: 0, name })
}

/// Maps a SQL type name string to a [`DataType`].
///
/// Used by the CAST parser to convert the `AS type` part into the
/// `DataType` variant stored in `Expr::Cast`.
fn parse_data_type(name: &str) -> Result<DataType, DbError> {
    match name.to_ascii_uppercase().as_str() {
        "INT" | "INTEGER" => Ok(DataType::Int),
        "BIGINT" => Ok(DataType::BigInt),
        "REAL" | "FLOAT" | "DOUBLE" => Ok(DataType::Real),
        "TEXT" | "VARCHAR" | "CHAR" | "STRING" => Ok(DataType::Text),
        "BOOL" | "BOOLEAN" => Ok(DataType::Bool),
        "BYTES" | "BLOB" | "BYTEA" => Ok(DataType::Bytes),
        "TIMESTAMP" => Ok(DataType::Timestamp),
        "DATE" => Ok(DataType::Date),
        "DECIMAL" | "NUMERIC" => Ok(DataType::Decimal),
        "UUID" => Ok(DataType::Uuid),
        other => Err(DbError::ParseError {
            message: format!("unknown type '{other}' in CAST"),
        }),
    }
}
