//! Expression sub-parser — parses [`Expr`] from the token stream.
//!
//! ## Operator precedence (lowest to highest, MySQL-compatible)
//!
//! ```text
//! expr           ::= or_expr
//! or_expr        ::= xor_expr (OR xor_expr)*
//! xor_expr       ::= and_expr (XOR and_expr)*
//! and_expr       ::= not_expr (AND not_expr)*
//! not_expr       ::= NOT not_expr | is_null_expr
//! is_null_expr   ::= predicate (IS [NOT] (NULL|TRUE|FALSE))?
//! predicate      ::= bitor_expr ([NOT] BETWEEN bitor_expr AND bitor_expr)
//!                  | bitor_expr [NOT] LIKE atom [ESCAPE atom]
//!                  | bitor_expr [NOT] IN '(' expr_list ')'
//!                  | bitor_expr [NOT] REGEXP bitor_expr
//!                  | bitor_expr '<=>' bitor_expr
//!                  | bitor_expr (cmp_op bitor_expr)?
//! bitor_expr     ::= bitand_expr ('|' bitand_expr)*
//! bitand_expr    ::= shift_expr ('&' shift_expr)*
//! shift_expr     ::= addition (('<<'|'>>') addition)*
//! addition       ::= multiplication (('+' | '-' | '||') multiplication)*
//! multiplication ::= bitxor_expr (('*'|'/'|'%'|DIV) bitxor_expr)*
//! bitxor_expr    ::= json_extract_text ('^' json_extract_text)*
//! json_extract_text ::= unary ('->>' unary)*
//! unary          ::= '-' unary | '~' unary | atom
//! atom           ::= literal | hex_lit | col_ref | fn_call | '(' expr ')'
//! col_ref        ::= identifier ['.' identifier]
//! fn_call        ::= identifier '(' ([*] | [expr (',' expr)*]) ')'
//! ```
//!
//! Phase 4.3 covered: literals, NOT, comparisons, AND, OR.
//! Phase 4.4 adds: IS NULL, BETWEEN, LIKE, IN, arithmetic, table.col, function calls.
//! Phase 4.G4 adds: XOR, REGEXP/RLIKE, <=>, bitwise (&|^~<<>>), DIV, hex literals.

use axiomdb_core::error::DbError;
use axiomdb_types::{DataType, Value};

use crate::{
    ast::{OrderByItem, SortOrder, WindowSpec},
    expr::{
        BinaryOp, Expr, SqlJsonOnBehavior, SqlJsonPathMode, SqlJsonQueryKind, SqlJsonQuotes,
        SqlJsonWrapper, UnaryOp, WindowFunc,
    },
    lexer::Token,
    session::normalize_collation_name,
};

// Helper: build a BinaryOp node.
#[inline]
fn binop(op: BinaryOp, left: Expr, right: Expr) -> Expr {
    Expr::BinaryOp {
        op,
        left: Box::new(left),
        right: Box::new(right),
    }
}

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
    let mut left = parse_xor(p)?;
    while p.eat(&Token::Or) {
        let right = parse_xor(p)?;
        left = binop(BinaryOp::Or, left, right);
    }
    Ok(left)
}

// ── XOR ───────────────────────────────────────────────────────────────────────

fn parse_xor(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_and(p)?;
    while p.eat(&Token::Xor) {
        let right = parse_and(p)?;
        left = binop(BinaryOp::Xor, left, right);
    }
    Ok(left)
}

// ── AND ───────────────────────────────────────────────────────────────────────

fn parse_and(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_not(p)?;
    while p.eat(&Token::And) {
        let right = parse_not(p)?;
        left = binop(BinaryOp::And, left, right);
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
        match p.peek() {
            Token::Null => {
                p.advance();
                return Ok(Expr::IsNull {
                    expr: Box::new(expr),
                    negated,
                });
            }
            Token::True => {
                p.advance();
                return Ok(Expr::IsBoolean {
                    expr: Box::new(expr),
                    value: true,
                    negated,
                });
            }
            Token::False => {
                p.advance();
                return Ok(Expr::IsBoolean {
                    expr: Box::new(expr),
                    value: false,
                    negated,
                });
            }
            // Phase 21.17 — IS [NOT] DISTINCT FROM: NULL-safe comparison.
            //   a IS DISTINCT FROM b     ≡ NOT (a <=> b)
            //   a IS NOT DISTINCT FROM b ≡ (a <=> b)
            Token::Distinct => {
                p.advance();
                p.expect(&Token::From)?;
                let rhs = parse_predicate(p)?;
                let eq = Expr::BinaryOp {
                    op: crate::expr::BinaryOp::NullSafe,
                    left: Box::new(expr),
                    right: Box::new(rhs),
                };
                // IS DISTINCT FROM     → NOT eq  (negated=false here means "distinct" so wrap)
                // IS NOT DISTINCT FROM → eq
                if negated {
                    return Ok(eq);
                }
                return Ok(Expr::UnaryOp {
                    op: crate::expr::UnaryOp::Not,
                    operand: Box::new(eq),
                });
            }
            _ => {
                return Err(DbError::ParseError {
                    message: "expected NULL, TRUE, FALSE, or DISTINCT after IS [NOT]".into(),
                    position: None,
                });
            }
        }
    }
    Ok(expr)
}

// ── Predicate: BETWEEN, LIKE, REGEXP, IN, <=>, comparison ────────────────────

fn parse_predicate(p: &mut Parser) -> Result<Expr, DbError> {
    let left_expr = parse_bitor(p)?;
    let left = parse_collate_suffix(p, left_expr)?;

    // Check for optional NOT before BETWEEN/LIKE/IN/REGEXP.
    let negated = if matches!(
        (p.peek(), p.peek_at(1)),
        (Token::Not, Token::Between)
            | (Token::Not, Token::Like)
            | (Token::Not, Token::In)
            | (Token::Not, Token::Regexp)
            | (Token::Not, Token::Rlike)
    ) {
        p.advance(); // consume NOT
        true
    } else {
        false
    };

    match p.peek() {
        Token::Between => {
            p.advance();
            let low = parse_bitor(p)?;
            p.expect(&Token::And)?;
            let high = parse_bitor(p)?;
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
            // Optional ESCAPE clause (4.4d) — single-char escape override.
            let escape = if p.eat(&Token::Escape) {
                Some(Box::new(parse_atom(p)?))
            } else {
                None
            };
            Ok(Expr::Like {
                expr: Box::new(left),
                pattern: Box::new(pattern),
                negated,
                escape,
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
        Token::Regexp | Token::Rlike => {
            p.advance();
            let pattern = parse_bitor(p)?;
            let op_expr = binop(BinaryOp::Regexp, left, pattern);
            if negated {
                Ok(Expr::UnaryOp {
                    op: UnaryOp::Not,
                    operand: Box::new(op_expr),
                })
            } else {
                Ok(op_expr)
            }
        }
        // Phase 20.15: PostgreSQL POSIX regex tilde operators (binary position only).
        // Token::Tilde in *prefix* position is still parsed as unary BitNot in parse_unary.
        Token::Tilde if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::RegexpTilde, left, right))
        }
        Token::TildeAsterisk if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::RegexpITilde, left, right))
        }
        Token::BangTilde if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::RegexpNotTilde, left, right))
        }
        Token::BangTildeAsterisk if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::RegexpNotITilde, left, right))
        }
        // Phase 11.17: `@>` JSONB containment — same precedence level as comparisons.
        Token::JsonContains if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::JsonContains, left, right))
        }
        // Phase 11.18a: `<@` JSONB contained-by — same level as `@>`.
        Token::JsonContainedBy if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::JsonContainedBy, left, right))
        }
        // Phase 20.4, Step 5: `&&` array overlap — same level as `@>` / `<@`.
        Token::AmpAmp if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::ArrayOverlap, left, right))
        }
        // Phase 11.21b: `@?` in infix position = JSONB JSONPath exists.
        Token::JsonbPathExists if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::JsonbPathExists, left, right))
        }
        // Phase 11.21c: `@@` in infix position = JSONB JSONPath match.
        // The same token is reserved for MySQL `@@session_var` prefixes at
        // atom position; because this arm runs only after a completed LHS,
        // there is no grammatical collision.
        Token::AtAt if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::JsonbPathMatch, left, right))
        }
        // Phase 11.18a: `?` in infix position = JSONB key/array-element exists.
        // `?` as a prefix atom stays reserved for prepared-statement
        // placeholders (see `parse_atom`), which is unreachable here.
        Token::Question if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::JsonExists, left, right))
        }
        // Phase 11.18b: `?|` = any-of, `?&` = all-of (JSONB-array RHS).
        Token::JsonExistsAny if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::JsonExistsAny, left, right))
        }
        Token::JsonExistsAll if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::JsonExistsAll, left, right))
        }
        // Phase 11.18c: #>, #>>, #-
        Token::JsonPathExtract if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::JsonPathExtract, left, right))
        }
        Token::JsonPathExtractText if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::JsonPathExtractText, left, right))
        }
        Token::JsonPathDelete if !negated => {
            p.advance();
            let right = parse_bitor(p)?;
            Ok(binop(BinaryOp::JsonPathDelete, left, right))
        }
        cmp if !negated => {
            let op = match cmp {
                Token::NullSafe => BinaryOp::NullSafe,
                Token::Eq => BinaryOp::Eq,
                Token::NotEq => BinaryOp::NotEq,
                Token::Lt => BinaryOp::Lt,
                Token::LtEq => BinaryOp::LtEq,
                Token::Gt => BinaryOp::Gt,
                Token::GtEq => BinaryOp::GtEq,
                _ => return Ok(left),
            };
            p.advance();
            let right = parse_bitor(p)?;

            // Phase 20.4, Step 7: Fix up AnyOf/AllOf placeholder with the actual LHS.
            // When parsing `lhs = ANY(array)`, the AnyOf node was created with
            // Literal(Null) as a placeholder. Now we have `left` available,
            // so we substitute it in.
            let right = match &right {
                Expr::AnyOf { expr, .. } if matches!(**expr, Expr::Literal(Value::Null)) => {
                    Expr::AnyOf {
                        expr: Box::new(left.clone()),
                        array: match right {
                            Expr::AnyOf { array, .. } => array,
                            _ => unreachable!(),
                        },
                    }
                }
                Expr::AllOf { expr, .. } if matches!(**expr, Expr::Literal(Value::Null)) => {
                    Expr::AllOf {
                        expr: Box::new(left.clone()),
                        array: match right {
                            Expr::AllOf { array, .. } => array,
                            _ => unreachable!(),
                        },
                    }
                }
                _ => right,
            };

            Ok(binop(op, left, right))
        }
        _ if negated => {
            // NOT was consumed but no BETWEEN/LIKE/IN/REGEXP followed — error.
            Err(DbError::ParseError {
                message: "expected BETWEEN, LIKE, IN, or REGEXP after NOT".into(),
                position: Some(p.current_pos()),
            })
        }
        _ => Ok(left),
    }
}

fn parse_collate_suffix(p: &mut Parser, mut expr: Expr) -> Result<Expr, DbError> {
    while p.eat_ident_ci("collate") {
        let name = p.parse_identifier()?;
        expr = Expr::Collate {
            expr: Box::new(expr),
            collation: normalize_collation_name(&name)?,
        };
    }
    Ok(expr)
}

// ── Bitwise OR: | ─────────────────────────────────────────────────────────────

fn parse_bitor(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_bitand(p)?;
    while p.eat(&Token::Pipe) {
        let right = parse_bitand(p)?;
        left = binop(BinaryOp::BitOr, left, right);
    }
    Ok(left)
}

// ── Bitwise AND: & ────────────────────────────────────────────────────────────

fn parse_bitand(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_shift(p)?;
    while p.eat(&Token::Amp) {
        let right = parse_shift(p)?;
        left = binop(BinaryOp::BitAnd, left, right);
    }
    Ok(left)
}

// ── Bit shifts: <<, >> ────────────────────────────────────────────────────────

fn parse_shift(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_addition(p)?;
    loop {
        let op = match p.peek() {
            Token::ShiftLeft => BinaryOp::ShiftLeft,
            Token::ShiftRight => BinaryOp::ShiftRight,
            _ => break,
        };
        p.advance();
        let right = parse_addition(p)?;
        left = binop(op, left, right);
    }
    Ok(left)
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

// ── Multiplication: *, /, DIV, % ─────────────────────────────────────────────

fn parse_multiplication(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_bitxor(p)?;
    loop {
        let op = match p.peek() {
            Token::Star => BinaryOp::Mul,
            Token::Slash => BinaryOp::Div,
            Token::Percent => BinaryOp::Mod,
            Token::IntDiv => BinaryOp::IntDiv,
            _ => break,
        };
        p.advance();
        let right = parse_bitxor(p)?;
        left = binop(op, left, right);
    }
    Ok(left)
}

// ── Bitwise XOR: ^ ───────────────────────────────────────────────────────────

fn parse_bitxor(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_json_extract_sub(p)?;
    while p.eat(&Token::Caret) {
        let right = parse_json_extract_sub(p)?;
        left = binop(BinaryOp::BitXor, left, right);
    }
    Ok(left)
}

// ── JSON sub-document extraction: -> (Phase 11.16) ───────────────────────────
//
// `expr -> 'key'`  returns the sub-document as JSONB (Value::Jsonb).
// `expr -> 0`      returns array element as JSONB.
// This level is higher precedence than `->>`  but only parses `->` tokens.
// The lexer emits `->>` before `->` so there's no ambiguity.

fn parse_json_extract_sub(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_json_extract_text(p)?;
    while p.eat(&Token::JsonExtractSub) {
        let right = parse_json_extract_text(p)?;
        left = binop(BinaryOp::JsonSub, left, right);
    }
    Ok(left)
}

// ── JSON field extraction: ->> (Phase 11.4) ──────────────────────────────────

fn parse_json_extract_text(p: &mut Parser) -> Result<Expr, DbError> {
    let mut left = parse_unary(p)?;
    while p.eat(&Token::JsonExtractText) {
        let path = normalize_json_extract_text_path(parse_unary(p)?);
        left = Expr::Function {
            name: "json_extract".to_string(),
            args: vec![left, path],
        };
    }
    Ok(left)
}

fn normalize_json_extract_text_path(path: Expr) -> Expr {
    match path {
        Expr::Literal(Value::Text(s)) if !s.starts_with('$') => {
            Expr::Literal(Value::Text(format!("$.{s}")))
        }
        other => other,
    }
}

// ── Unary: unary minus, bitwise NOT ──────────────────────────────────────────

fn parse_unary(p: &mut Parser) -> Result<Expr, DbError> {
    if p.eat(&Token::Minus) {
        let operand = parse_unary(p)?;
        return Ok(Expr::UnaryOp {
            op: UnaryOp::Neg,
            operand: Box::new(operand),
        });
    }
    if p.eat(&Token::Tilde) {
        let operand = parse_unary(p)?;
        return Ok(Expr::UnaryOp {
            op: UnaryOp::BitNot,
            operand: Box::new(operand),
        });
    }
    parse_postfix(p)
}

// ── Postfix operators: subscript, field access ────────────────────────────────

/// Parses postfix operators applied to an expression:
/// - `expr[index]` — array subscript (1-indexed)
/// - `expr[lo:hi]` — array slice
/// - `expr[lo:hi]` with only one bound — partial slice
///
/// This is called after the base expression has been parsed.
fn parse_postfix(p: &mut Parser) -> Result<Expr, DbError> {
    let mut expr = parse_atom(p)?;

    // Handle subscript: expr[ index or lo:hi ]
    while p.eat(&Token::LBracket) {
        // Check if this is a slice: [lo:hi] or [:hi] or [lo:]
        if matches!(p.peek(), Token::Colon) || matches!(p.peek_at(1), Token::Colon) {
            // Slice syntax: [lo:hi], [:hi], or [lo:]
            // Note: when we enter here, we've already consumed the '['
            let lo = if matches!(p.peek(), Token::Colon) {
                // [:hi] — lo defaults to 1
                Some(Box::new(Expr::Literal(Value::Int(1))))
            } else {
                // [lo:...] — parse lo
                Some(Box::new(parse_expr(p)?))
            };

            // Expect and consume ':'
            p.expect(&Token::Colon)?;

            // Parse hi
            let hi = if matches!(p.peek(), Token::RBracket) {
                // [lo:] — hi defaults to array length (we'll use a large number)
                // Since we don't know the length at parse time, we use a sentinel.
                // The evaluator will handle clamping.
                Some(Box::new(Expr::Literal(Value::BigInt(i64::MAX))))
            } else {
                Some(Box::new(parse_expr(p)?))
            };

            p.expect(&Token::RBracket)?;

            expr = Expr::Subscript {
                array: Box::new(expr),
                index: lo.unwrap_or_else(|| Box::new(Expr::Literal(Value::Int(1)))),
                slice: hi,
            };
        } else {
            // Regular subscript: [index]
            let index = parse_expr(p)?;
            p.expect(&Token::RBracket)?;
            expr = Expr::Subscript {
                array: Box::new(expr),
                index: Box::new(index),
                slice: None,
            };
        }
    }

    // Handle AT TIME ZONE: `expr AT TIME ZONE tz_expr`
    // Lowered to __at_time_zone(expr, tz_expr) to avoid a new AST node.
    if p.eat_ident_ci("AT") {
        if p.eat_ident_ci("TIME") && p.eat_ident_ci("ZONE") {
            let tz_expr = parse_expr(p)?;
            expr = Expr::Function {
                name: "__at_time_zone".to_string(),
                args: vec![expr, tz_expr],
            };
        } else {
            return Err(DbError::ParseError {
                message: "expected TIME ZONE after AT".into(),
                position: Some(p.current_pos()),
            });
        }
    }

    // Handle PostgreSQL-style type cast: `expr::type[]`
    // This must appear AFTER subscript handling (which consumes LBracket/RBracket).
    // We check for :: specifically to avoid interfering with slice [lo:hi] syntax.
    if matches!(p.peek(), Token::Colon) && matches!(p.peek_at(1), Token::Colon) {
        p.advance(); // consume first ':'
        p.advance(); // consume second ':'
        let parsed = super::ddl::parse_data_type(p)?;
        // Construct the proper DataType with array dimensions.
        // For `int[]`, ndims=1 and data_type=Int → DataType::Array(Int)
        // For `int[][]`, ndims=2 → DataType::Array(Array(Int))
        use axiomdb_types::DataType;
        let mut target = parsed.data_type;
        for _ in 0..parsed.ndims {
            target = DataType::Array(Box::new(target));
        }
        expr = Expr::Cast {
            expr: Box::new(expr),
            target,
        };
    }

    Ok(expr)
}

// ── Atom ──────────────────────────────────────────────────────────────────────

fn parse_atom(p: &mut Parser) -> Result<Expr, DbError> {
    let pos = p.current_pos();

    match p.peek().clone() {
        // `?` — positional prepared-statement parameter placeholder.
        Token::Question => {
            p.advance();
            let idx = p.param_count;
            p.param_count += 1;
            Ok(Expr::Param { idx })
        }

        Token::HexLit(n) => {
            p.advance();
            Ok(Expr::Literal(Value::BigInt(n)))
        }
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
        Token::Default => {
            p.advance();
            Ok(Expr::Default)
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
        // `VALUES(col)` inside an `ON DUPLICATE KEY UPDATE` assignment list
        // — MySQL pseudo-function pointing at the proposed row's value for
        // `col`. Only a bare identifier is accepted; complex expressions
        // inside `VALUES(...)` are a MariaDB parse error and we match that.
        Token::Values if p.in_odku_assignment => {
            p.advance();
            p.expect(&Token::LParen)?;
            let col_name = p.parse_identifier()?;
            p.expect(&Token::RParen)?;
            Ok(Expr::InsertValue {
                col_idx: 0,
                name: col_name,
            })
        }

        // ── ARRAY[...] constructor (Phase 20.4) ───────────────────────────────
        // Parse ARRAY[expr, ...] — PostgreSQL-compatible array constructor.
        // This fires when we see ARRAY keyword/ident followed by '['.
        Token::Array => {
            p.advance();
            parse_array_constructor(p)
        }

        // Ident "ARRAY" followed by '[' is also an array constructor.
        // Check this BEFORE the general Token::Ident case to avoid the
        // pattern being shadowed by Token::Ident(_).
        Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("ARRAY") => {
            p.advance();
            if matches!(p.peek(), Token::LBracket) {
                parse_array_constructor(p)
            } else {
                // ARRAY as a bare identifier (e.g., type name) — fall through
                Ok(Expr::Column { col_idx: 0, name: s.to_string() })
            }
        }

        // ── ANY/ALL array constructs (Phase 20.4, Step 7) ──────────────────
        // `expr = ANY(array)` / `expr = ALL(array)`.
        // The ANY/ALL keyword appears after the comparison operator.
        // When we see ANY/ALL here, we parse the parenthesized content and
        // signal to the comparison handler that it needs to build the proper
        // AnyOf/AllOf node with the left-hand side.
        Token::Any | Token::Some => {
            p.advance();
            p.expect(&Token::LParen)?;
            // ANY(SELECT ...) → subquery
            if matches!(p.peek(), Token::Select) {
                let query = parse_subquery(p)?;
                p.expect(&Token::RParen)?;
                // Signal subquery by returning a special marker.
                // The cmp arm detects this and converts to InSubquery with the LHS.
                return Ok(Expr::InSubquery {
                    expr: Box::new(Expr::Literal(Value::Null)),
                    query: Box::new(query),
                    negated: false,
                });
            }
            // ANY(array) — parse array expression
            let arr_expr = parse_expr(p)?;
            p.expect(&Token::RParen)?;
            Ok(Expr::AnyOf {
                expr: Box::new(Expr::Literal(Value::Null)), // placeholder; cmp handler fixes this
                array: Box::new(arr_expr),
            })
        }

        Token::All => {
            p.advance();
            p.expect(&Token::LParen)?;
            // ALL(SELECT ...) — subquery
            if matches!(p.peek(), Token::Select) {
                let query = parse_subquery(p)?;
                p.expect(&Token::RParen)?;
                return Ok(Expr::InSubquery {
                    expr: Box::new(Expr::Literal(Value::Null)),
                    query: Box::new(query),
                    negated: true, // ALL = NOT (NOT IN)
                });
            }
            // ALL(array)
            let arr_expr = parse_expr(p)?;
            p.expect(&Token::RParen)?;
            Ok(Expr::AllOf {
                expr: Box::new(Expr::Literal(Value::Null)), // placeholder; cmp handler fixes this
                array: Box::new(arr_expr),
            })
        }

        // unnest(array_expr) — set-returning function; parsed as a regular Function
        // call so the SRF expansion in the executor can detect it by name.
        Token::Unnest => {
            p.advance();
            p.expect(&Token::LParen)?;
            let arg = parse_expr(p)?;
            p.expect(&Token::RParen)?;
            Ok(Expr::Function {
                name: "unnest".to_string(),
                args: vec![arg],
            })
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
        | Token::Autocommit
        | Token::Type
        | Token::Enum
        | Token::Regexp
        | Token::Rlike
        | Token::Xor
        | Token::IntDiv
        // Reserved DML keywords that double as MySQL built-in function names.
        | Token::Truncate  // TRUNCATE(x, d) — numeric rounding function
        | Token::Insert    // INSERT(str, pos, len, newstr) — string replacement
        | Token::Merge
        => parse_ident_or_call(p),

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
                        "CASE requires at least one WHEN branch, found {:?}",
                        p.peek(),
                    ),
                    position: Some(p.current_pos()),
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

        // ROW(expr, ...) — composite value constructor (Phase 20.18).
        // Token::Row is a keyword token; handle it here before the `other` fallback.
        Token::Row => {
            p.advance();
            p.expect(&Token::LParen)?;
            let mut elems = Vec::new();
            if !matches!(p.peek(), Token::RParen) {
                elems.push(parse_expr(p)?);
                while p.eat(&Token::Comma) {
                    elems.push(parse_expr(p)?);
                }
            }
            p.expect(&Token::RParen)?;
            Ok(Expr::Row(elems))
        }

        other => Err(DbError::ParseError {
            message: format!("unexpected token {:?} in expression", other,),
            position: Some(pos),
        }),
    }
}

/// Parse an identifier (possibly `table.col`) or a function call.
fn parse_ident_or_call(p: &mut Parser) -> Result<Expr, DbError> {
    let name = p.parse_identifier()?;

    // Check for table.column: `name.field`
    if p.eat(&Token::Dot) {
        let field = p.parse_identifier()?;
        if p.in_on_conflict_expr && name.eq_ignore_ascii_case("excluded") {
            return Ok(Expr::ExcludedValue {
                col_idx: 0,
                name: field,
            });
        }
        let qualified = format!("{name}.{field}");
        // No function call after table.col in Phase 4.4
        return Ok(Expr::Column {
            col_idx: 0,
            name: qualified,
        });
    }

    // Phase 11.19a — SQL:2016 `JSON_VALUE` / `JSON_QUERY` / `JSON_EXISTS`
    // are special-form expressions with a keyword-driven grammar, not
    // variadic function calls. We detect them here before the regular
    // `name(` function dispatcher consumes the LParen.
    {
        let lower = name.to_ascii_lowercase();
        let sql_json_kind = match lower.as_str() {
            "json_value" => Some(SqlJsonQueryKind::Value),
            "json_query" => Some(SqlJsonQueryKind::Query),
            "json_exists" => Some(SqlJsonQueryKind::Exists),
            _ => None,
        };
        if let Some(kind) = sql_json_kind {
            if matches!(p.peek(), Token::LParen) {
                return parse_sql_json_query(p, kind);
            }
        }
    }

    // Phase 20.20 — SQL/XML constructor special forms.
    {
        let lower = name.to_ascii_lowercase();
        if matches!(p.peek(), Token::LParen) {
            match lower.as_str() {
                "xmlelement" => return parse_xmlelement(p),
                "xmlforest" => return parse_xmlforest(p),
                "xmlroot" => return parse_xmlroot(p),
                "xmlconcat" => return parse_xmlconcat(p),
                "xmlquery" => return parse_xmlquery(p),
                _ => {}
            }
        }
    }

    // SQL niladic-keyword functions — no parens required in the standard.
    // Without this, `CURRENT_TIMESTAMP`, `CURRENT_DATE`, `CURRENT_USER`, etc.
    // would parse as column references and fail at eval time.
    {
        let lower = name.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "current_timestamp"
                | "current_date"
                | "current_time"
                | "current_user"
                | "session_user"
                | "system_user"
                | "current_schema"
                | "current_database"
                | "localtimestamp"
                | "localtime"
        ) && !matches!(p.peek(), Token::LParen)
        {
            return Ok(Expr::Function {
                name: lower,
                args: vec![],
            });
        }
    }

    // Check for function call: `name(`
    if p.eat(&Token::LParen) {
        // ── GROUP_CONCAT special syntax ───────────────────────────────────────
        // GROUP_CONCAT([DISTINCT] expr [ORDER BY e [ASC|DESC] [, ...]] [SEPARATOR 'str'])
        if name.eq_ignore_ascii_case("group_concat") {
            let distinct = p.eat(&Token::Distinct);
            let expr = parse_expr(p)?;

            // Optional ORDER BY inside GROUP_CONCAT.
            let mut order_by: Vec<(Expr, SortOrder)> = Vec::new();
            if p.eat(&Token::Order) {
                p.expect(&Token::By)?;
                loop {
                    let ob_expr = parse_expr(p)?;
                    let dir = if p.eat(&Token::Asc) {
                        SortOrder::Asc
                    } else if p.eat(&Token::Desc) {
                        SortOrder::Desc
                    } else {
                        SortOrder::Asc
                    };
                    order_by.push((ob_expr, dir));
                    // Stop if the next token is SEPARATOR, RParen, or not a Comma.
                    if !p.eat(&Token::Comma) {
                        break;
                    }
                    // After consuming the comma, peek — stop if SEPARATOR or RParen.
                    if matches!(p.peek(), Token::Separator | Token::RParen) {
                        break;
                    }
                }
            }

            // Optional SEPARATOR 'string'.
            let separator = if p.eat(&Token::Separator) {
                match p.peek().clone() {
                    Token::StringLit(s) => {
                        p.advance();
                        s
                    }
                    other => {
                        return Err(DbError::ParseError {
                            message: format!(
                                "expected string literal after SEPARATOR, got {:?}",
                                other
                            ),
                            position: Some(p.current_pos()),
                        })
                    }
                }
            } else {
                ",".to_string()
            };

            p.expect(&Token::RParen)?;
            return Ok(Expr::GroupConcat {
                expr: Box::new(expr),
                distinct,
                order_by,
                separator,
            });
        }

        // ── ARRAY_AGG (PostgreSQL-compatible) ──────────────────────────────────
        // array_agg(expr [ORDER BY e [ASC|DESC], ...] [DISTINCT])
        if name.eq_ignore_ascii_case("array_agg") {
            let distinct = p.eat(&Token::Distinct);
            let expr = parse_expr(p)?;

            // Optional ORDER BY inside array_agg.
            let mut order_by: Vec<(Expr, SortOrder)> = Vec::new();
            if p.eat(&Token::Order) {
                p.expect(&Token::By)?;
                loop {
                    let ob_expr = parse_expr(p)?;
                    let dir = if p.eat(&Token::Asc) {
                        SortOrder::Asc
                    } else if p.eat(&Token::Desc) {
                        SortOrder::Desc
                    } else {
                        SortOrder::Asc
                    };
                    order_by.push((ob_expr, dir));
                    if !p.eat(&Token::Comma) {
                        break;
                    }
                    if matches!(p.peek(), Token::RParen) {
                        break;
                    }
                }
            }

            p.expect(&Token::RParen)?;
            return Ok(Expr::ArrayAgg {
                expr: Box::new(expr),
                distinct,
                order_by,
            });
        }

        // ── STRING_AGG alias (PostgreSQL-compatible) ───────────────────────────
        // string_agg(expr, separator) — equivalent to GROUP_CONCAT(expr SEPARATOR sep)
        if name.eq_ignore_ascii_case("string_agg") {
            let expr = parse_expr(p)?;
            p.expect(&Token::Comma)?;
            // The separator must be a string literal at parse time.
            let separator = match p.peek().clone() {
                Token::StringLit(s) => {
                    p.advance();
                    s
                }
                other => {
                    return Err(DbError::ParseError {
                        message: format!(
                            "string_agg separator must be a string literal, got {:?}",
                            other
                        ),
                        position: Some(p.current_pos()),
                    })
                }
            };
            p.expect(&Token::RParen)?;
            return Ok(Expr::GroupConcat {
                expr: Box::new(expr),
                distinct: false,
                order_by: vec![],
                separator,
            });
        }

        // ── GROUPING(...) — SQL standard Phase 21.21 ─────────────────────────
        // GROUPING(expr [, expr, ...])
        // Returns a bitmask indicating which expressions are "rolled up" in the
        // current grouping set. `universe_indices` is left `None` here and
        // populated by the analyzer.
        if name.eq_ignore_ascii_case("grouping") {
            let mut args = Vec::new();
            if !matches!(p.peek(), Token::RParen) {
                loop {
                    args.push(parse_expr(p)?);
                    if !p.eat(&Token::Comma) {
                        break;
                    }
                }
            }
            p.expect(&Token::RParen)?;
            return Ok(Expr::Grouping {
                args,
                universe_indices: None,
            });
        }

        // CAST(expr AS type) — special syntax, not a regular function call.
        if name.eq_ignore_ascii_case("cast") {
            let expr = parse_expr(p)?;
            if !p.eat(&Token::As) {
                return Err(DbError::ParseError {
                    message: "expected AS in CAST".into(),
                    position: Some(p.current_pos()),
                });
            }
            let (target, _, _) =
                super::ddl::parse_data_type(p).map(|p| (p.data_type, p.type_len, p.is_char))?;
            p.expect(&Token::RParen)?;
            return Ok(Expr::Cast {
                expr: Box::new(expr),
                target,
            });
        }

        // CONVERT(expr, type) or CONVERT(expr USING charset) — MySQL syntax (4.19g).
        // Both forms are desugared to Expr::Cast; USING form maps to Text.
        // Exception: CONVERT(money_expr, 'CURRENCY_CODE') — Phase 20.17 money conversion.
        if name.eq_ignore_ascii_case("convert") {
            let expr = parse_expr(p)?;
            if p.eat(&Token::Using) {
                // CONVERT(expr USING charset_name) — consume charset name, cast to Text.
                // charset name may be an identifier or a keyword (utf8, binary, etc.)
                p.advance(); // consume whatever the charset token is
                p.expect(&Token::RParen)?;
                return Ok(Expr::Cast {
                    expr: Box::new(expr),
                    target: DataType::Text,
                });
            }
            p.expect(&Token::Comma)?;
            // Phase 20.17: CONVERT(money_expr, 'USD') → money currency conversion.
            if let Token::StringLit(currency) = p.peek().clone() {
                p.advance();
                p.expect(&Token::RParen)?;
                return Ok(Expr::Function {
                    name: "convert_currency".into(),
                    args: vec![expr, Expr::Literal(Value::Text(currency))],
                });
            }
            let target = parse_convert_type(p)?;
            p.expect(&Token::RParen)?;
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

        // COUNT(DISTINCT col) / SUM(DISTINCT col) / AVG(DISTINCT col) — 4.9f
        let name_lower = name.to_ascii_lowercase();
        if matches!(name_lower.as_str(), "count" | "sum" | "avg" | "min" | "max")
            && p.eat(&Token::Distinct)
        {
            let arg = parse_expr(p)?;
            p.expect(&Token::RParen)?;
            // MIN(DISTINCT) / MAX(DISTINCT) desugar to MIN/MAX (DISTINCT has no effect).
            let effective_name = match name_lower.as_str() {
                "min" | "max" => name_lower.clone(),
                _ => format!("{name_lower}_distinct"),
            };
            return Ok(Expr::Function {
                name: effective_name,
                args: vec![arg],
            });
        }

        // TIMESTAMPDIFF(unit, ts1, ts2) — first arg is a bare date unit keyword
        // (SECOND, MINUTE, HOUR, DAY, WEEK, MONTH, YEAR, MICROSECOND).
        // We parse it as a string literal to avoid "ColumnNotFound: DAY" from the analyzer.
        let name_lower = name.to_ascii_lowercase();
        if name_lower == "timestampdiff" && !matches!(p.peek(), Token::RParen) {
            let unit_str = p.parse_identifier()?;
            let mut args = vec![Expr::Literal(Value::Text(unit_str.to_ascii_uppercase()))];
            while p.eat(&Token::Comma) {
                args.push(parse_expr(p)?);
            }
            p.expect(&Token::RParen)?;
            return Ok(Expr::Function {
                name: name_lower,
                args,
            });
        }

        // ROW(expr, ...) — composite value constructor (Phase 20.18).
        if name.eq_ignore_ascii_case("row") {
            let mut elems = Vec::new();
            if !matches!(p.peek(), Token::RParen) {
                elems.push(parse_expr(p)?);
                while p.eat(&Token::Comma) {
                    elems.push(parse_expr(p)?);
                }
            }
            p.expect(&Token::RParen)?;
            return Ok(Expr::Row(elems));
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
        let function_expr = Expr::Function {
            name: name.to_ascii_lowercase(),
            args,
        };
        if p.eat(&Token::Over) {
            return parse_window_call(p, function_expr);
        }
        return Ok(function_expr);
    }

    // Plain column reference
    Ok(Expr::Column { col_idx: 0, name })
}

fn parse_window_call(p: &mut Parser, function_expr: Expr) -> Result<Expr, DbError> {
    let (func, func_name) = match function_expr {
        Expr::Function { ref name, ref args } if name.eq_ignore_ascii_case("row_number") => {
            if !args.is_empty() {
                return Err(DbError::ParseError {
                    message: "ROW_NUMBER() does not take arguments".into(),
                    position: Some(p.current_pos()),
                });
            }
            (WindowFunc::RowNumber, "ROW_NUMBER")
        }
        Expr::Function { ref name, ref args } if name.eq_ignore_ascii_case("rank") => {
            if !args.is_empty() {
                return Err(DbError::ParseError {
                    message: "RANK() does not take arguments".into(),
                    position: Some(p.current_pos()),
                });
            }
            (WindowFunc::Rank, "RANK")
        }
        Expr::Function { ref name, ref args } if name.eq_ignore_ascii_case("dense_rank") => {
            if !args.is_empty() {
                return Err(DbError::ParseError {
                    message: "DENSE_RANK() does not take arguments".into(),
                    position: Some(p.current_pos()),
                });
            }
            (WindowFunc::DenseRank, "DENSE_RANK")
        }
        Expr::Function { name, .. } => {
            return Err(DbError::NotImplemented {
                feature: format!("window function `{name}` — ranking MVP only"),
            });
        }
        _ => {
            return Err(DbError::ParseError {
                message: "OVER can only follow a function call".into(),
                position: Some(p.current_pos()),
            });
        }
    };

    p.expect(&Token::LParen)?;

    let mut partition_by = Vec::new();
    if p.eat(&Token::Partition) {
        p.expect(&Token::By)?;
        loop {
            partition_by.push(parse_expr(p)?);
            if !p.eat(&Token::Comma) {
                break;
            }
        }
    }

    let mut order_by = Vec::new();
    if p.eat(&Token::Order) {
        p.expect(&Token::By)?;
        order_by.push(parse_window_order_item(p)?);
        while p.eat(&Token::Comma) {
            order_by.push(parse_window_order_item(p)?);
        }
    }

    if order_by.is_empty() {
        return Err(DbError::ParseError {
            message: format!("{func_name}() OVER (...) requires ORDER BY"),
            position: Some(p.current_pos()),
        });
    }

    if matches!(p.peek(), Token::Rows)
        || matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("RANGE") || s.eq_ignore_ascii_case("GROUPS"))
    {
        return Err(DbError::NotImplemented {
            feature: "window frame clauses — ranking MVP only".into(),
        });
    }

    p.expect(&Token::RParen)?;

    Ok(Expr::Window {
        func,
        spec: WindowSpec {
            partition_by,
            order_by,
        },
    })
}

fn parse_window_order_item(p: &mut Parser) -> Result<OrderByItem, DbError> {
    let expr = parse_expr(p)?;
    let order = if p.eat(&Token::Asc) {
        SortOrder::Asc
    } else if p.eat(&Token::Desc) {
        SortOrder::Desc
    } else {
        SortOrder::Asc
    };
    let nulls = if p.eat(&Token::Nulls) {
        if p.eat(&Token::First) {
            Some(crate::ast::NullsOrder::First)
        } else if p.eat(&Token::Last) {
            Some(crate::ast::NullsOrder::Last)
        } else {
            return Err(DbError::ParseError {
                message: "expected FIRST or LAST after NULLS".into(),
                position: Some(p.current_pos()),
            });
        }
    } else {
        None
    };
    Ok(OrderByItem { expr, order, nulls })
}

/// Parses the type argument for `CONVERT(expr, type)` — MySQL-specific type names
/// that are not standard SQL data types (SIGNED, UNSIGNED, BINARY, JSON…).
fn parse_convert_type(p: &mut Parser) -> Result<DataType, DbError> {
    // Handle MySQL-specific CONVERT type keywords that aren't standard SQL types.
    match p.peek().clone() {
        Token::Ident(s) => {
            let lower = s.to_ascii_lowercase();
            match lower.as_str() {
                "signed" => {
                    p.advance();
                    // SIGNED [INTEGER] — optional INTEGER keyword
                    if let Token::TyInt | Token::TyInteger = p.peek() {
                        p.advance();
                    }
                    Ok(DataType::BigInt)
                }
                "unsigned" => {
                    p.advance();
                    // UNSIGNED [INTEGER]
                    if let Token::TyInt | Token::TyInteger = p.peek() {
                        p.advance();
                    }
                    Ok(DataType::BigInt)
                }
                "binary" => {
                    p.advance();
                    // Consume optional (N) length specifier.
                    if p.eat(&Token::LParen) {
                        p.advance(); // integer length
                        p.expect(&Token::RParen)?;
                    }
                    Ok(DataType::Bytes)
                }
                "json" => {
                    p.advance();
                    Ok(DataType::Text)
                }
                _ => super::ddl::parse_data_type(p).map(|p| p.data_type),
            }
        }
        _ => super::ddl::parse_data_type(p).map(|p| p.data_type),
    }
}

// ── Phase 11.19a — SQL:2016 JSON query special forms ────────────────────────-

/// Eat ARRAY if it appears as either the keyword token or an identifier.
fn eat_array_tok(p: &mut Parser<'_>) -> bool {
    match p.peek() {
        Token::Array => {
            p.pos += 1;
            true
        }
        Token::Ident(s) | Token::QuotedIdent(s) if s.eq_ignore_ascii_case("ARRAY") => {
            p.pos += 1;
            true
        }
        _ => false,
    }
}

/// Parses the body of `JSON_VALUE(...)`, `JSON_QUERY(...)`, or
/// `JSON_EXISTS(...)` after the caller has consumed the function name.
/// Keyword-driven grammar — ordering:
///
/// ```text
///   doc , path
///     [ RETURNING <type> ]                   -- not for JSON_EXISTS
///     [ ON EMPTY { ERROR | NULL | DEFAULT expr } ]  -- not for JSON_EXISTS
///     [ ON ERROR { ERROR | NULL | DEFAULT expr | TRUE | FALSE | UNKNOWN } ]
/// ```
fn parse_sql_json_query(p: &mut Parser, kind: SqlJsonQueryKind) -> Result<Expr, DbError> {
    p.expect(&Token::LParen)?;
    let doc = parse_expr(p)?;
    p.expect(&Token::Comma)?;

    // Path must be a string literal; we parse it once to split the
    // `strict`/`lax` mode prefix from the actual jsonpath body.
    let path_raw = match p.peek().clone() {
        Token::StringLit(s) => {
            p.advance();
            s
        }
        other => {
            return Err(DbError::ParseError {
                message: format!("SQL/JSON path must be a string literal, got {other:?}"),
                position: Some(p.current_pos()),
            })
        }
    };
    let (path_mode, path_str) = split_sql_json_path_mode(&path_raw);

    let mut returning: Option<DataType> = None;
    let mut wrapper = SqlJsonWrapper::Without;
    let mut quotes = SqlJsonQuotes::Keep;
    let mut on_empty = SqlJsonOnBehavior::Null;
    let mut on_error = match kind {
        SqlJsonQueryKind::Exists => SqlJsonOnBehavior::FalseLit,
        _ => SqlJsonOnBehavior::Null,
    };
    let mut passing: Vec<(Expr, String)> = Vec::new();

    // Optional PASSING <expr> AS <name> [, ...]  — Phase 11.19c.
    // Placed before RETERURNING per SQL:2016 grammar.
    if p.eat_ident_ci("PASSING") {
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
                Token::Ident(n) => {
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
            passing.push((val_expr, var_name.to_string()));
            if !matches!(p.peek(), Token::Comma) {
                break;
            }
            p.advance();
        }
    }

    // Optional RETURNING <type> — not valid for JSON_EXISTS.
    // NOTE: RETURNING is a reserved keyword → Token::Returning, not Token::Ident,
    // so eat_ident_ci("RETURNING") never fires; use eat(&Token::Returning).
    if p.eat(&Token::Returning) {
        if matches!(kind, SqlJsonQueryKind::Exists) {
            return Err(DbError::ParseError {
                message: "RETURNING is not allowed on JSON_EXISTS".into(),
                position: Some(p.current_pos()),
            });
        }
        let (dt, _, _) =
            super::ddl::parse_data_type(p).map(|p| (p.data_type, p.type_len, p.is_char))?;
        returning = Some(dt);
    }

    // Optional WRAPPER / QUOTES — JSON_QUERY only (Phase 11.19b).
    // Grammar: [WITH [UNCONDITIONAL|CONDITIONAL] [ARRAY] WRAPPER
    //         | WITHOUT [ARRAY] WRAPPER]
    //         [KEEP|OMIT QUOTES [ON SCALAR STRING]]
    let saw_with = matches!(p.peek(), Token::With);
    if saw_with {
        p.advance();
    }
    if saw_with {
        if !matches!(kind, SqlJsonQueryKind::Query) {
            return Err(DbError::ParseError {
                message: "WITH ... WRAPPER is only valid on JSON_QUERY".into(),
                position: Some(p.current_pos()),
            });
        }
        let kind_w = if p.eat_ident_ci("CONDITIONAL") {
            SqlJsonWrapper::Conditional
        } else {
            let _ = p.eat_ident_ci("UNCONDITIONAL");
            SqlJsonWrapper::Unconditional
        };
        let _ = eat_array_tok(p);
        if !p.eat_ident_ci("WRAPPER") {
            return Err(DbError::ParseError {
                message: "expected WRAPPER after WITH [CONDITIONAL|UNCONDITIONAL] [ARRAY]".into(),
                position: Some(p.current_pos()),
            });
        }
        wrapper = kind_w;
    } else if p.eat_ident_ci("WITHOUT") {
        if !matches!(kind, SqlJsonQueryKind::Query) {
            return Err(DbError::ParseError {
                message: "WITHOUT ... WRAPPER is only valid on JSON_QUERY".into(),
                position: Some(p.current_pos()),
            });
        }
        let _ = eat_array_tok(p);
        if !p.eat_ident_ci("WRAPPER") {
            return Err(DbError::ParseError {
                message: "expected WRAPPER after WITHOUT [ARRAY]".into(),
                position: Some(p.current_pos()),
            });
        }
        wrapper = SqlJsonWrapper::Without;
    }

    if p.eat_ident_ci("KEEP") {
        if !matches!(kind, SqlJsonQueryKind::Query) {
            return Err(DbError::ParseError {
                message: "KEEP QUOTES is only valid on JSON_QUERY".into(),
                position: Some(p.current_pos()),
            });
        }
        if !p.eat_ident_ci("QUOTES") {
            return Err(DbError::ParseError {
                message: "expected QUOTES after KEEP".into(),
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
        quotes = SqlJsonQuotes::Keep;
    } else if p.eat_ident_ci("OMIT") {
        if !matches!(kind, SqlJsonQueryKind::Query) {
            return Err(DbError::ParseError {
                message: "OMIT QUOTES is only valid on JSON_QUERY".into(),
                position: Some(p.current_pos()),
            });
        }
        if !p.eat_ident_ci("QUOTES") {
            return Err(DbError::ParseError {
                message: "expected QUOTES after OMIT".into(),
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
        quotes = SqlJsonQuotes::Omit;
    }

    // Optional ON EMPTY <behavior> — not valid for JSON_EXISTS.
    while matches!(p.peek(), Token::On) {
        p.advance(); // ON
        if p.eat_ident_ci("EMPTY") {
            if matches!(kind, SqlJsonQueryKind::Exists) {
                return Err(DbError::ParseError {
                    message: "ON EMPTY is not allowed on JSON_EXISTS".into(),
                    position: Some(p.current_pos()),
                });
            }
            on_empty = parse_sql_json_behavior(p, /*is_exists=*/ false)?;
            continue;
        }
        if p.eat_ident_ci("ERROR") {
            on_error = parse_sql_json_behavior(p, matches!(kind, SqlJsonQueryKind::Exists))?;
            continue;
        }
        return Err(DbError::ParseError {
            message: "expected EMPTY or ERROR after ON".into(),
            position: Some(p.current_pos()),
        });
    }

    p.expect(&Token::RParen)?;

    Ok(Expr::SqlJsonQuery {
        kind,
        doc: Box::new(doc),
        path: path_str,
        path_mode,
        passing,
        returning,
        wrapper,
        quotes,
        on_empty,
        on_error,
    })
}

/// Parses the right-hand-side of an `ON EMPTY` / `ON ERROR` clause.
fn parse_sql_json_behavior(p: &mut Parser, is_exists: bool) -> Result<SqlJsonOnBehavior, DbError> {
    // ERROR keyword stays an Ident in logos (no Token::Error reserved).
    if p.eat_ident_ci("ERROR") {
        return Ok(SqlJsonOnBehavior::Error);
    }
    if p.eat(&Token::Null) {
        return Ok(SqlJsonOnBehavior::Null);
    }
    if is_exists {
        if p.eat(&Token::True) {
            return Ok(SqlJsonOnBehavior::TrueLit);
        }
        if p.eat(&Token::False) {
            return Ok(SqlJsonOnBehavior::FalseLit);
        }
        if p.eat_ident_ci("UNKNOWN") {
            return Ok(SqlJsonOnBehavior::Unknown);
        }
    }
    if p.eat(&Token::Default) {
        let e = parse_expr(p)?;
        return Ok(SqlJsonOnBehavior::Default(Box::new(e)));
    }
    Err(DbError::ParseError {
        message: if is_exists {
            "expected ERROR | NULL | TRUE | FALSE | UNKNOWN | DEFAULT expr".into()
        } else {
            "expected ERROR | NULL | DEFAULT expr".into()
        },
        position: Some(p.current_pos()),
    })
}

/// Splits the `strict ` / `lax ` mode prefix from the rest of the path.
fn split_sql_json_path_mode(raw: &str) -> (SqlJsonPathMode, String) {
    let trimmed = raw.trim_start();
    if let Some(rest) = trimmed.strip_prefix("strict ") {
        (SqlJsonPathMode::Strict, rest.trim_start().to_string())
    } else if let Some(rest) = trimmed.strip_prefix("lax ") {
        (SqlJsonPathMode::Lax, rest.trim_start().to_string())
    } else {
        // SQL:2016 + PG default is strict.
        (SqlJsonPathMode::Strict, trimmed.to_string())
    }
}

// ── Phase 20.4 — ARRAY constructor ────────────────────────────────────────────

/// Parses `ARRAY[expr, ...]` — PostgreSQL-compatible array constructor.
///
/// Expects `ARRAY` to have already been consumed. Parses the opening `[`,
/// then comma-separated expressions until `]`.
fn parse_array_constructor(p: &mut Parser) -> Result<Expr, DbError> {
    p.expect(&Token::LBracket)?;

    // Empty array: ARRAY[]
    if matches!(p.peek(), Token::RBracket) {
        p.advance();
        return Ok(Expr::ArrayConstructor { elements: vec![] });
    }

    // Parse elements
    let mut elements = vec![parse_expr(p)?];
    while p.eat(&Token::Comma) {
        elements.push(parse_expr(p)?);
    }

    p.expect(&Token::RBracket)?;

    Ok(Expr::ArrayConstructor { elements })
}

// ── Phase 20.20 — SQL/XML constructor parsers ─────────────────────────────────

/// `XMLELEMENT(NAME tag [, XMLATTRIBUTES(v AS name, ...) ] [, content ...])`
fn parse_xmlelement(p: &mut Parser) -> Result<Expr, DbError> {
    p.expect(&Token::LParen)?;

    // NAME keyword
    let name_kw = p.parse_identifier()?;
    if !name_kw.eq_ignore_ascii_case("NAME") {
        return Err(DbError::ParseError {
            message: format!("expected NAME in XMLELEMENT, got '{name_kw}'"),
            position: Some(p.current_pos()),
        });
    }

    let tag = p.parse_identifier()?;
    let mut attrs: Vec<(Expr, String)> = Vec::new();
    let mut content: Vec<Expr> = Vec::new();

    while p.eat(&Token::Comma) {
        // Check for XMLATTRIBUTES(...)
        if let Token::Ident(s) = p.peek().clone() {
            if s.eq_ignore_ascii_case("XMLATTRIBUTES") {
                p.advance();
                p.expect(&Token::LParen)?;
                loop {
                    let v_expr = parse_expr(p)?;
                    p.expect(&Token::As)?;
                    let a_name = p.parse_identifier()?;
                    attrs.push((v_expr, a_name));
                    if !p.eat(&Token::Comma) {
                        break;
                    }
                }
                p.expect(&Token::RParen)?;
                continue;
            }
        }
        // Otherwise it's a content expression
        content.push(parse_expr(p)?);
    }

    p.expect(&Token::RParen)?;
    Ok(Expr::XmlElement {
        tag,
        attrs,
        content,
    })
}

/// `XMLFOREST(expr AS name [, ...])`
fn parse_xmlforest(p: &mut Parser) -> Result<Expr, DbError> {
    p.expect(&Token::LParen)?;
    let mut items: Vec<(Expr, String)> = Vec::new();

    if !matches!(p.peek(), Token::RParen) {
        loop {
            let expr = parse_expr(p)?;
            p.expect(&Token::As)?;
            let name = p.parse_identifier()?;
            items.push((expr, name));
            if !p.eat(&Token::Comma) {
                break;
            }
        }
    }

    p.expect(&Token::RParen)?;
    Ok(Expr::XmlForest { items })
}

/// `XMLROOT(xml_expr, VERSION string [, STANDALONE YES|NO|NO VALUE])`
fn parse_xmlroot(p: &mut Parser) -> Result<Expr, DbError> {
    p.expect(&Token::LParen)?;

    let doc = Box::new(parse_expr(p)?);
    p.expect(&Token::Comma)?;

    // VERSION keyword
    let ver_kw = p.parse_identifier()?;
    if !ver_kw.eq_ignore_ascii_case("VERSION") {
        return Err(DbError::ParseError {
            message: format!("expected VERSION in XMLROOT, got '{ver_kw}'"),
            position: Some(p.current_pos()),
        });
    }

    let version = match p.peek().clone() {
        Token::StringLit(s) => {
            p.advance();
            s
        }
        other => {
            return Err(DbError::ParseError {
                message: format!("expected version string in XMLROOT, got {other:?}"),
                position: Some(p.current_pos()),
            })
        }
    };

    let mut standalone: Option<bool> = None;
    if p.eat(&Token::Comma) {
        let sa_kw = p.parse_identifier()?;
        if !sa_kw.eq_ignore_ascii_case("STANDALONE") {
            return Err(DbError::ParseError {
                message: format!("expected STANDALONE in XMLROOT, got '{sa_kw}'"),
                position: Some(p.current_pos()),
            });
        }
        match p.peek().clone() {
            Token::Ident(s) if s.eq_ignore_ascii_case("YES") => {
                p.advance();
                standalone = Some(true);
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("NO") => {
                p.advance();
                // Check for "NO VALUE"
                if let Token::Ident(s2) = p.peek().clone() {
                    if s2.eq_ignore_ascii_case("VALUE") {
                        p.advance();
                        standalone = None;
                    } else {
                        standalone = Some(false);
                    }
                } else {
                    standalone = Some(false);
                }
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!(
                        "expected YES, NO, or NO VALUE after STANDALONE, got {other:?}"
                    ),
                    position: Some(p.current_pos()),
                })
            }
        }
    }

    p.expect(&Token::RParen)?;
    Ok(Expr::XmlRoot {
        doc,
        version,
        standalone,
    })
}

/// `XMLCONCAT(xml1 [, xml2, ...])`
fn parse_xmlconcat(p: &mut Parser) -> Result<Expr, DbError> {
    p.expect(&Token::LParen)?;
    let mut args: Vec<Expr> = Vec::new();

    if !matches!(p.peek(), Token::RParen) {
        args.push(parse_expr(p)?);
        while p.eat(&Token::Comma) {
            args.push(parse_expr(p)?);
        }
    }

    p.expect(&Token::RParen)?;
    Ok(Expr::XmlConcat { args })
}

/// `XMLQUERY(xpath_string PASSING xml_expr [RETURNING CONTENT])`
fn parse_xmlquery(p: &mut Parser) -> Result<Expr, DbError> {
    p.expect(&Token::LParen)?;

    let xpath = match p.peek().clone() {
        Token::StringLit(s) => {
            p.advance();
            s
        }
        other => {
            return Err(DbError::ParseError {
                message: format!("expected XPath string in XMLQUERY, got {other:?}"),
                position: Some(p.current_pos()),
            })
        }
    };

    // PASSING keyword
    let passing_kw = p.parse_identifier()?;
    if !passing_kw.eq_ignore_ascii_case("PASSING") {
        return Err(DbError::ParseError {
            message: format!("expected PASSING in XMLQUERY, got '{passing_kw}'"),
            position: Some(p.current_pos()),
        });
    }

    let doc = Box::new(parse_expr(p)?);

    // Optional RETURNING CONTENT
    if let Token::Ident(s) = p.peek().clone() {
        if s.eq_ignore_ascii_case("RETURNING") {
            p.advance();
            let content_kw = p.parse_identifier()?;
            if !content_kw.eq_ignore_ascii_case("CONTENT") {
                return Err(DbError::ParseError {
                    message: format!(
                        "expected CONTENT after RETURNING in XMLQUERY, got '{content_kw}'"
                    ),
                    position: Some(p.current_pos()),
                });
            }
        }
    }

    p.expect(&Token::RParen)?;
    Ok(Expr::XmlQuery { xpath, doc })
}
