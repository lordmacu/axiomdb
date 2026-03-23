//! DML statement parsers — SELECT, INSERT, UPDATE, DELETE.

use nexusdb_core::error::DbError;

use nexusdb_types::Value;

use crate::{
    ast::{
        Assignment, DeleteStmt, FromClause, InsertSource, InsertStmt, JoinClause, JoinCondition,
        JoinType, NullsOrder, OrderByItem, SelectItem, SelectStmt, SortOrder, Stmt, UpdateStmt,
    },
    expr::Expr,
    lexer::Token,
};

use super::{expr::parse_expr, Parser};

/// Parse a DML statement. Called by `Parser::parse_stmt`.
pub(crate) fn parse_dml(p: &mut Parser) -> Result<Stmt, DbError> {
    match p.peek() {
        Token::Select => {
            p.advance();
            parse_select(p).map(Stmt::Select)
        }
        Token::Insert => {
            p.advance();
            parse_insert(p)
        }
        Token::Update => {
            p.advance();
            parse_update(p)
        }
        Token::Delete => {
            p.advance();
            parse_delete(p)
        }
        other => Err(DbError::ParseError {
            message: format!(
                "expected SELECT, INSERT, UPDATE, or DELETE, found {:?} at position {}",
                other,
                p.current_pos()
            ),
        }),
    }
}

// ── SELECT ────────────────────────────────────────────────────────────────────

/// Parses everything after `SELECT` has been consumed.
pub(crate) fn parse_select(p: &mut Parser) -> Result<SelectStmt, DbError> {
    let distinct = p.eat(&Token::Distinct);
    let columns = parse_select_list(p)?;

    let from = if p.eat(&Token::From) {
        Some(parse_from_item(p)?)
    } else {
        None
    };

    let joins = if from.is_some() {
        parse_join_clauses(p)?
    } else {
        vec![]
    };

    let where_clause = if p.eat(&Token::Where) {
        Some(parse_expr(p)?)
    } else {
        None
    };

    let group_by = if p.eat(&Token::Group) {
        p.expect(&Token::By)?;
        parse_expr_list(p)?
    } else {
        vec![]
    };

    let having = if p.eat(&Token::Having) {
        Some(parse_expr(p)?)
    } else {
        None
    };

    let order_by = if p.eat(&Token::Order) {
        p.expect(&Token::By)?;
        parse_order_items(p)?
    } else {
        vec![]
    };

    let (limit, offset) = parse_limit_offset(p)?;

    Ok(SelectStmt {
        distinct,
        columns,
        from,
        joins,
        where_clause,
        group_by,
        having,
        order_by,
        limit,
        offset,
    })
}

// ── SELECT list ───────────────────────────────────────────────────────────────

fn parse_select_list(p: &mut Parser) -> Result<Vec<SelectItem>, DbError> {
    let mut items = vec![parse_select_item(p)?];
    while p.eat(&Token::Comma) {
        items.push(parse_select_item(p)?);
    }
    Ok(items)
}

fn parse_select_item(p: &mut Parser) -> Result<SelectItem, DbError> {
    // `*` — bare wildcard
    if p.eat(&Token::Star) {
        return Ok(SelectItem::Wildcard);
    }

    // `identifier.*` — qualified wildcard
    // Detect: current = Ident, next = Dot, after = Star
    if matches!(p.peek(), Token::Ident(_) | Token::QuotedIdent(_))
        && p.peek_at(1) == &Token::Dot
        && p.peek_at(2) == &Token::Star
    {
        let name = p.parse_identifier()?;
        p.advance(); // Dot
        p.advance(); // Star
        return Ok(SelectItem::QualifiedWildcard(name));
    }

    // General expression, optionally aliased with AS
    let expr = parse_expr(p)?;
    let alias = if p.eat(&Token::As) {
        // Allow keywords as aliases after explicit AS
        Some(parse_alias(p)?)
    } else {
        None
    };

    Ok(SelectItem::Expr { expr, alias })
}

/// Parse an alias — allows certain keywords as alias names.
fn parse_alias(p: &mut Parser) -> Result<String, DbError> {
    match p.peek().clone() {
        Token::Ident(s) | Token::QuotedIdent(s) | Token::DqIdent(s) => {
            p.advance();
            Ok(s.to_string())
        }
        // Allow unreserved keywords as aliases
        Token::Key => {
            p.advance();
            Ok("key".into())
        }
        Token::Index => {
            p.advance();
            Ok("index".into())
        }
        Token::Tables => {
            p.advance();
            Ok("tables".into())
        }
        Token::Desc => {
            p.advance();
            Ok("desc".into())
        }
        Token::Action => {
            p.advance();
            Ok("action".into())
        }
        Token::Names => {
            p.advance();
            Ok("names".into())
        }
        Token::Autocommit => {
            p.advance();
            Ok("autocommit".into())
        }
        other => Err(DbError::ParseError {
            message: format!(
                "expected alias name after AS, found {:?} at position {}",
                other,
                p.current_pos()
            ),
        }),
    }
}

// ── FROM clause ───────────────────────────────────────────────────────────────

fn parse_from_item(p: &mut Parser) -> Result<FromClause, DbError> {
    // Subquery: `(SELECT ...) AS alias`
    if p.eat(&Token::LParen) {
        p.expect(&Token::Select)?;
        let sub = parse_select(p)?;
        p.expect(&Token::RParen)?;
        p.eat(&Token::As);
        let alias = p.parse_identifier()?;
        return Ok(FromClause::Subquery {
            query: Box::new(sub),
            alias,
        });
    }

    // Regular table reference
    let mut table_ref = p.parse_table_ref()?;

    // Optional alias: `AS name` or implicit `name` (if next is a plain identifier,
    // not a keyword like JOIN, WHERE, ON, etc.)
    if p.eat(&Token::As) || is_implicit_alias_token(p.peek()) {
        table_ref.alias = Some(p.parse_identifier()?);
    }

    Ok(FromClause::Table(table_ref))
}

/// Returns true if the current token can start an implicit table alias
/// (a plain identifier, not a SQL keyword that starts a new clause).
fn is_implicit_alias_token(tok: &Token) -> bool {
    matches!(
        tok,
        Token::Ident(_) | Token::QuotedIdent(_) | Token::DqIdent(_)
    )
}

// ── JOIN clauses ──────────────────────────────────────────────────────────────

fn parse_join_clauses(p: &mut Parser) -> Result<Vec<JoinClause>, DbError> {
    let mut joins = Vec::new();
    loop {
        let join_type = match p.peek() {
            Token::Join => {
                p.advance();
                JoinType::Inner
            }
            Token::Inner => {
                p.advance();
                p.expect(&Token::Join)?;
                JoinType::Inner
            }
            Token::Left => {
                p.advance();
                p.eat(&Token::Outer); // OUTER is optional
                p.expect(&Token::Join)?;
                JoinType::Left
            }
            Token::Right => {
                p.advance();
                p.eat(&Token::Outer);
                p.expect(&Token::Join)?;
                JoinType::Right
            }
            Token::Full => {
                p.advance();
                p.eat(&Token::Outer);
                p.expect(&Token::Join)?;
                JoinType::Full
            }
            Token::Cross => {
                p.advance();
                p.expect(&Token::Join)?;
                JoinType::Cross
            }
            _ => break,
        };

        let table = parse_from_item(p)?;

        let condition = match p.peek() {
            Token::On => {
                p.advance();
                JoinCondition::On(parse_expr(p)?)
            }
            Token::Using => {
                p.advance();
                p.expect(&Token::LParen)?;
                let mut cols = vec![p.parse_identifier()?];
                while p.eat(&Token::Comma) {
                    cols.push(p.parse_identifier()?);
                }
                p.expect(&Token::RParen)?;
                JoinCondition::Using(cols)
            }
            other => {
                // CROSS JOIN has no condition; others require one
                if join_type == JoinType::Cross {
                    // No condition for CROSS JOIN — use a dummy ON TRUE
                    JoinCondition::On(Expr::Literal(Value::Bool(true)))
                } else {
                    return Err(DbError::ParseError {
                        message: format!(
                            "expected ON or USING after JOIN table, found {:?} at position {}",
                            other,
                            p.current_pos()
                        ),
                    });
                }
            }
        };

        joins.push(JoinClause {
            join_type,
            table,
            condition,
        });
    }
    Ok(joins)
}

// ── ORDER BY ──────────────────────────────────────────────────────────────────

fn parse_order_items(p: &mut Parser) -> Result<Vec<OrderByItem>, DbError> {
    let mut items = vec![parse_order_item(p)?];
    while p.eat(&Token::Comma) {
        items.push(parse_order_item(p)?);
    }
    Ok(items)
}

fn parse_order_item(p: &mut Parser) -> Result<OrderByItem, DbError> {
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
            Some(NullsOrder::First)
        } else if p.eat(&Token::Last) {
            Some(NullsOrder::Last)
        } else {
            return Err(DbError::ParseError {
                message: format!(
                    "expected FIRST or LAST after NULLS at position {}",
                    p.current_pos()
                ),
            });
        }
    } else {
        None
    };
    Ok(OrderByItem { expr, order, nulls })
}

// ── LIMIT / OFFSET ────────────────────────────────────────────────────────────

fn parse_limit_offset(p: &mut Parser) -> Result<(Option<Expr>, Option<Expr>), DbError> {
    if p.eat(&Token::Limit) {
        let limit = parse_expr(p)?;
        let offset = if p.eat(&Token::Offset) {
            Some(parse_expr(p)?)
        } else {
            None
        };
        Ok((Some(limit), offset))
    } else {
        Ok((None, None))
    }
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn parse_expr_list(p: &mut Parser) -> Result<Vec<Expr>, DbError> {
    let mut exprs = vec![parse_expr(p)?];
    while p.eat(&Token::Comma) {
        exprs.push(parse_expr(p)?);
    }
    Ok(exprs)
}

// ── INSERT ────────────────────────────────────────────────────────────────────

fn parse_insert(p: &mut Parser) -> Result<Stmt, DbError> {
    p.expect(&Token::Into)?;
    let table = p.parse_table_ref()?;

    // Optional column list
    let columns: Option<Vec<String>> = if p.eat(&Token::LParen) {
        let mut cols = vec![p.parse_identifier()?];
        while p.eat(&Token::Comma) {
            cols.push(p.parse_identifier()?);
        }
        p.expect(&Token::RParen)?;
        Some(cols)
    } else {
        None
    };

    let source = match p.peek() {
        Token::Values => {
            p.advance();
            let mut rows: Vec<Vec<Expr>> = Vec::new();
            loop {
                p.expect(&Token::LParen)?;
                let mut row = vec![parse_expr(p)?];
                while p.eat(&Token::Comma) {
                    row.push(parse_expr(p)?);
                }
                p.expect(&Token::RParen)?;
                rows.push(row);
                if !p.eat(&Token::Comma) {
                    break;
                }
            }
            InsertSource::Values(rows)
        }
        Token::Default => {
            p.advance();
            p.expect(&Token::Values)?;
            InsertSource::DefaultValues
        }
        Token::Select => {
            p.advance();
            let select = parse_select(p)?;
            InsertSource::Select(Box::new(select))
        }
        other => {
            return Err(DbError::ParseError {
                message: format!(
                "expected VALUES, DEFAULT VALUES, or SELECT in INSERT, found {:?} at position {}",
                other,
                p.current_pos()
            ),
            })
        }
    };

    Ok(Stmt::Insert(InsertStmt {
        table,
        columns,
        source,
    }))
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

fn parse_update(p: &mut Parser) -> Result<Stmt, DbError> {
    let table = p.parse_table_ref()?;
    p.expect(&Token::Set)?;

    let mut assignments = vec![parse_assignment(p)?];
    while p.eat(&Token::Comma) {
        assignments.push(parse_assignment(p)?);
    }

    let where_clause = if p.eat(&Token::Where) {
        Some(parse_expr(p)?)
    } else {
        None
    };

    Ok(Stmt::Update(UpdateStmt {
        table,
        assignments,
        where_clause,
    }))
}

fn parse_assignment(p: &mut Parser) -> Result<Assignment, DbError> {
    let column = p.parse_identifier()?;
    p.expect(&Token::Eq)?;
    let value = parse_expr(p)?;
    Ok(Assignment { column, value })
}

// ── DELETE ────────────────────────────────────────────────────────────────────

fn parse_delete(p: &mut Parser) -> Result<Stmt, DbError> {
    p.expect(&Token::From)?;
    let table = p.parse_table_ref()?;
    let where_clause = if p.eat(&Token::Where) {
        Some(parse_expr(p)?)
    } else {
        None
    };
    Ok(Stmt::Delete(DeleteStmt {
        table,
        where_clause,
    }))
}
