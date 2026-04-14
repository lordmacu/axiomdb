//! DML statement parsers — SELECT, INSERT, UPDATE, DELETE.

use axiomdb_core::error::DbError;

use axiomdb_types::Value;

use crate::{
    ast::{
        Assignment, DeleteStmt, FromClause, InsertSource, InsertStmt, JoinClause, JoinCondition,
        JoinType, LockMode, NullsOrder, OrderByItem, SelectItem, SelectStmt, SetOpKind, SetOpTail,
        SortOrder, Stmt, UpdateStmt,
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
            let first = parse_select(p)?;
            // Check for UNION / INTERSECT / EXCEPT continuation.
            if matches!(p.peek(), Token::Union | Token::Intersect | Token::Except) {
                return parse_set_op(p, first);
            }
            Ok(Stmt::Select(first))
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
                "expected SELECT, INSERT, UPDATE, or DELETE, found {:?}",
                other,
            ),
            position: Some(p.current_pos()),
        }),
    }
}

// ── SET OPERATIONS (UNION / INTERSECT / EXCEPT) ───────────────────────────────

/// Parses `(UNION|INTERSECT|EXCEPT) [ALL] SELECT ...` chains after the first
/// SELECT has been parsed. Left-associative. Each tail carries its own kind
/// and ALL flag (MySQL 8.0.31+ compatible).
fn parse_set_op(p: &mut Parser, first: SelectStmt) -> Result<Stmt, DbError> {
    let mut rest = Vec::new();

    loop {
        let kind = match p.peek() {
            Token::Union => SetOpKind::Union,
            Token::Intersect => SetOpKind::Intersect,
            Token::Except => SetOpKind::Except,
            _ => break,
        };
        p.advance();

        let all = p.eat(&Token::All);
        p.expect(&Token::Select)?;
        let select = parse_select(p)?;
        rest.push(SetOpTail { kind, all, select });
    }

    Ok(Stmt::SetOp { first, rest })
}

// ── SELECT ────────────────────────────────────────────────────────────────────

/// Consume an identifier that matches `keyword` (case-insensitive).
/// Returns true and advances if matched, false otherwise.
fn eat_ident_ci(p: &mut Parser, keyword: &str) -> bool {
    if let Token::Ident(s) = p.peek() {
        if s.eq_ignore_ascii_case(keyword) {
            p.advance();
            return true;
        }
    }
    false
}

/// Parses everything after `SELECT` has been consumed.
pub(crate) fn parse_select(p: &mut Parser) -> Result<SelectStmt, DbError> {
    let distinct = p.eat(&Token::Distinct);

    // MySQL optimizer hint modifiers (4.4i): consume and discard.
    // SQL_CALC_FOUND_ROWS is stored for FOUND_ROWS() support (4.5e).
    let mut calc_found_rows = false;
    loop {
        if eat_ident_ci(p, "SQL_CALC_FOUND_ROWS") {
            calc_found_rows = true;
        } else if eat_ident_ci(p, "HIGH_PRIORITY")
            || eat_ident_ci(p, "STRAIGHT_JOIN")
            || eat_ident_ci(p, "SQL_SMALL_RESULT")
            || eat_ident_ci(p, "SQL_BIG_RESULT")
            || eat_ident_ci(p, "SQL_BUFFER_RESULT")
        {
            // discard
        } else {
            break;
        }
    }

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

    let (group_by, with_rollup) = if p.eat(&Token::Group) {
        p.expect(&Token::By)?;
        let exprs = parse_expr_list(p)?;
        // Optional `WITH ROLLUP` modifier (MySQL + SQL std).
        let rollup = if matches!(p.peek(), Token::With)
            && matches!(p.peek_at(1), Token::Ident(s) if s.eq_ignore_ascii_case("rollup"))
        {
            p.advance(); // WITH
            p.advance(); // ROLLUP
            true
        } else {
            false
        };
        (exprs, rollup)
    } else {
        (vec![], false)
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

    // `FOR UPDATE` or `LOCK IN SHARE MODE` (row-level lock hint, ignored until Phase 13.7)
    let lock_mode = if p.eat(&Token::For) {
        p.expect(&Token::Update)?;
        Some(LockMode::ForUpdate)
    } else if matches!(p.peek(), Token::Lock) {
        p.advance(); // LOCK
        p.expect(&Token::In)?;
        p.expect(&Token::Share)?;
        p.expect(&Token::Mode)?;
        Some(LockMode::ShareMode)
    } else {
        None
    };

    Ok(SelectStmt {
        distinct,
        calc_found_rows,
        columns,
        from,
        joins,
        where_clause,
        group_by,
        with_rollup,
        having,
        order_by,
        limit,
        offset,
        lock_mode,
    })
}

// ── SELECT list ───────────────────────────────────────────────────────────────

/// Phase 21.4 — parse optional `RETURNING <select_item>[, ...]` tail.
/// Returns empty vec when `RETURNING` not present.
fn parse_returning_clause(p: &mut Parser) -> Result<Vec<SelectItem>, DbError> {
    if !p.eat(&Token::Returning) {
        return Ok(Vec::new());
    }
    parse_select_list(p)
}

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
        Token::Ident(s) | Token::QuotedIdent(s) => {
            p.advance();
            Ok(s.to_string())
        }
        Token::DqIdent(s) => {
            p.advance();
            Ok(s)
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
            message: format!("expected alias name after AS, found {:?}", other,),
            position: Some(p.current_pos()),
        }),
    }
}

// ── FROM clause ───────────────────────────────────────────────────────────────

fn parse_from_item(p: &mut Parser) -> Result<FromClause, DbError> {
    // Phase 11.20d3: accept optional `LATERAL` keyword before JSON_TABLE
    // or a subquery. PG-compatible syntactic sugar; no-op today because
    // correlated `doc` / PASSING on JSON_TABLE is enabled unconditionally
    // and correlated subqueries remain out of scope.
    p.eat(&Token::Lateral);

    // Subquery: `(SELECT ...) AS alias`  or  `(VALUES (...)) AS alias(cols)`.
    if p.eat(&Token::LParen) {
        // Phase 21.22 — `(VALUES (row), (row), ...) [AS] alias [(col, col, ...)]`.
        if p.eat(&Token::Values) {
            let mut rows: Vec<Vec<crate::expr::Expr>> = Vec::new();
            loop {
                p.expect(&Token::LParen)?;
                let mut row = vec![parse_expr(p)?];
                while p.eat(&Token::Comma) {
                    row.push(parse_expr(p)?);
                }
                p.expect(&Token::RParen)?;
                if let Some(first) = rows.first() {
                    if row.len() != first.len() {
                        return Err(DbError::ParseError {
                            message: format!(
                                "VALUES: row width {} does not match first row width {}",
                                row.len(),
                                first.len(),
                            ),
                            position: Some(p.current_pos()),
                        });
                    }
                }
                rows.push(row);
                if !p.eat(&Token::Comma) {
                    break;
                }
            }
            p.expect(&Token::RParen)?;
            p.eat(&Token::As);
            let alias = p.parse_identifier()?;
            let column_names = if p.eat(&Token::LParen) {
                let mut cols = vec![p.parse_identifier()?];
                while p.eat(&Token::Comma) {
                    cols.push(p.parse_identifier()?);
                }
                p.expect(&Token::RParen)?;
                Some(cols)
            } else {
                None
            };
            return Ok(FromClause::Values(Box::new(crate::ast::ValuesClause {
                rows,
                alias,
                column_names,
            })));
        }
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

    // Phase 11.20a — `JSON_TABLE(...)` table-valued function. Only dispatch
    // when followed by `(` so a user table named `json_table` without
    // arguments still parses as a plain table reference.
    if let Token::Ident(s) = p.peek() {
        if s.eq_ignore_ascii_case("JSON_TABLE") && matches!(p.peek_at(1), Token::LParen) {
            p.advance(); // consume the JSON_TABLE identifier
            return crate::parser::json_table::parse_json_table_call(p);
        }
    }

    // Phase 11.25a — PostgreSQL JSONB set-returning functions in FROM.
    if let Token::Ident(s) = p.peek() {
        if let Some(kind) = crate::ast::JsonbSrfKind::from_fn_name(s) {
            if matches!(p.peek_at(1), Token::LParen) {
                p.advance(); // consume function name
                p.expect(&Token::LParen)?;
                let doc = crate::parser::expr::parse_expr(p)?;
                p.expect(&Token::RParen)?;
                let alias = if p.eat(&Token::As) || is_implicit_alias_token(p.peek()) {
                    Some(p.parse_identifier()?)
                } else {
                    None
                };
                return Ok(FromClause::JsonbSrf(Box::new(crate::ast::JsonbSrf {
                    kind,
                    doc,
                    alias,
                })));
            }
        }
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
        // Phase 11.20d2: CROSS APPLY / OUTER APPLY are non-correlated lateral
        // sugar for INNER/LEFT JOIN … ON TRUE. Desugar at parse time.
        let apply_form = match (p.peek(), p.peek_at(1)) {
            (Token::Cross, Token::Apply) => Some(JoinType::Inner),
            (Token::Outer, Token::Apply) => Some(JoinType::Left),
            _ => None,
        };
        if let Some(join_type) = apply_form {
            p.advance(); // CROSS / OUTER
            p.advance(); // APPLY
            let table = parse_from_item(p)?;
            // APPLY takes no ON / USING — any such clause is a parse error.
            if matches!(p.peek(), Token::On | Token::Using) {
                return Err(DbError::ParseError {
                    message: "CROSS APPLY / OUTER APPLY does not accept ON / USING".into(),
                    position: Some(p.current_pos()),
                });
            }
            joins.push(JoinClause {
                join_type,
                table,
                condition: JoinCondition::On(Expr::Literal(Value::Bool(true))),
                natural: false,
            });
            continue;
        }

        // Phase 21.18 — optional NATURAL prefix.
        let natural = p.eat(&Token::Natural);

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
                if natural {
                    return Err(DbError::ParseError {
                        message: "NATURAL CROSS JOIN is invalid; CROSS has no match condition"
                            .into(),
                        position: Some(p.current_pos()),
                    });
                }
                p.advance();
                p.expect(&Token::Join)?;
                JoinType::Cross
            }
            _ => {
                if natural {
                    return Err(DbError::ParseError {
                        message: "expected JOIN after NATURAL [INNER|LEFT|RIGHT|FULL]".into(),
                        position: Some(p.current_pos()),
                    });
                }
                break;
            }
        };

        let table = parse_from_item(p)?;

        let condition = if natural {
            // Phase 21.18 — NATURAL JOIN forbids ON / USING.
            if matches!(p.peek(), Token::On | Token::Using) {
                return Err(DbError::ParseError {
                    message: "NATURAL JOIN does not accept ON / USING".into(),
                    position: Some(p.current_pos()),
                });
            }
            // Placeholder — analyzer fills the shared-column list.
            JoinCondition::Using(Vec::new())
        } else {
            match p.peek() {
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
                        JoinCondition::On(Expr::Literal(Value::Bool(true)))
                    } else {
                        return Err(DbError::ParseError {
                            message: format!(
                                "expected ON or USING after JOIN table, found {:?}",
                                other,
                            ),
                            position: Some(p.current_pos()),
                        });
                    }
                }
            }
        };

        joins.push(JoinClause {
            join_type,
            table,
            condition,
            natural,
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
                message: "expected FIRST or LAST after NULLS".into(),
                position: Some(p.current_pos()),
            });
        }
    } else {
        None
    };
    Ok(OrderByItem { expr, order, nulls })
}

// ── LIMIT / OFFSET ────────────────────────────────────────────────────────────

fn parse_limit_offset(p: &mut Parser) -> Result<(Option<Expr>, Option<Expr>), DbError> {
    let mut limit: Option<Expr> = None;
    let mut offset: Option<Expr> = None;
    let mut saw_limit_keyword = false;
    let mut saw_fetch_keyword = false;

    loop {
        if p.eat(&Token::Limit) {
            if saw_fetch_keyword {
                return Err(DbError::ParseError {
                    message: "cannot mix LIMIT with FETCH FIRST in the same query".into(),
                    position: Some(p.current_pos()),
                });
            }
            saw_limit_keyword = true;
            let first = parse_expr(p)?;
            if p.eat(&Token::Comma) {
                // MySQL comma syntax: LIMIT offset, count
                let count = parse_expr(p)?;
                offset = Some(first);
                limit = Some(count);
            } else {
                limit = Some(first);
                if p.eat(&Token::Offset) {
                    offset = Some(parse_expr(p)?);
                    // Optional SQL:2008 noise words after OFFSET.
                    let _ = p.eat(&Token::Row) || p.eat(&Token::Rows);
                }
            }
        } else if p.eat(&Token::Offset) {
            // Phase 21.19 — SQL:2008 standalone `OFFSET n [ ROW | ROWS ]`.
            let e = parse_expr(p)?;
            offset = Some(e);
            let _ = p.eat(&Token::Row) || p.eat(&Token::Rows);
        } else if p.eat(&Token::Fetch) {
            // Phase 21.19 — SQL:2008 `FETCH { FIRST | NEXT } [n] { ROW | ROWS } ONLY`.
            if saw_limit_keyword {
                return Err(DbError::ParseError {
                    message: "cannot mix LIMIT with FETCH FIRST in the same query".into(),
                    position: Some(p.current_pos()),
                });
            }
            saw_fetch_keyword = true;
            if !(p.eat(&Token::First) || p.eat(&Token::Next)) {
                return Err(DbError::ParseError {
                    message: "expected FIRST or NEXT after FETCH".into(),
                    position: Some(p.current_pos()),
                });
            }
            // Optional count; absent → 1 row.
            let count = if matches!(p.peek(), Token::Row | Token::Rows) {
                Expr::Literal(axiomdb_types::Value::Int(1))
            } else {
                parse_expr(p)?
            };
            if !(p.eat(&Token::Row) || p.eat(&Token::Rows)) {
                return Err(DbError::ParseError {
                    message: "expected ROW or ROWS after FETCH FIRST [count]".into(),
                    position: Some(p.current_pos()),
                });
            }
            p.expect(&Token::Only)?;
            limit = Some(count);
        } else {
            break;
        }
    }
    Ok((limit, offset))
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn parse_expr_list(p: &mut Parser) -> Result<Vec<Expr>, DbError> {
    let mut exprs = vec![parse_expr(p)?];
    while p.eat(&Token::Comma) {
        exprs.push(parse_expr(p)?);
    }
    Ok(exprs)
}

/// Parse a single argument to `CALL proc(arg1, arg2, ...)`.
/// Exposed as `pub(crate)` for use by the top-level parser dispatch.
pub(crate) fn parse_call_arg(p: &mut Parser) -> Result<Expr, DbError> {
    parse_expr(p)
}

/// Parse the expression after `DO`.
pub(crate) fn parse_do_expr(p: &mut Parser) -> Result<Expr, DbError> {
    parse_expr(p)
}

// ── INSERT / REPLACE ──────────────────────────────────────────────────────────

fn parse_insert(p: &mut Parser) -> Result<Stmt, DbError> {
    parse_insert_body(p, /*is_replace=*/ false)
}

/// Entry point for `REPLACE INTO` (MySQL upsert). The caller has already
/// consumed the leading `REPLACE` identifier.
pub(crate) fn parse_replace(p: &mut Parser) -> Result<Stmt, DbError> {
    parse_insert_body(p, /*is_replace=*/ true)
}

fn parse_insert_body(p: &mut Parser, is_replace: bool) -> Result<Stmt, DbError> {
    // MySQL priority / delay modifiers — consume and discard (4.6e).
    // Accepted for both INSERT and REPLACE.
    eat_ident_ci(p, "LOW_PRIORITY");
    eat_ident_ci(p, "HIGH_PRIORITY");
    eat_ident_ci(p, "DELAYED");
    // `INSERT IGNORE INTO ...` — silently skip constraint violations.
    // `REPLACE IGNORE` is not valid MySQL — reject up front.
    let ignore = p.eat(&Token::Ignore);
    if is_replace && ignore {
        return Err(DbError::ParseError {
            message: "REPLACE IGNORE is not valid MySQL syntax".into(),
            position: Some(p.current_pos()),
        });
    }
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

    // `INSERT INTO t SET col=val, ...` — MySQL assignment syntax.
    // Only valid when no explicit column list was given (columns.is_none()).
    if columns.is_none() && matches!(p.peek(), Token::Set) {
        p.advance(); // consume SET
        let mut col_names: Vec<String> = Vec::new();
        let mut col_values: Vec<Expr> = Vec::new();
        loop {
            let col = p.parse_identifier()?;
            p.expect(&Token::Eq)?;
            let val = parse_expr(p)?;
            col_names.push(col);
            col_values.push(val);
            if !p.eat(&Token::Comma) {
                break;
            }
        }
        let on_duplicate_update = parse_on_duplicate_update_tail(p, is_replace)?;
        let returning = parse_returning_clause(p)?;
        return Ok(Stmt::Insert(InsertStmt {
            table,
            columns: Some(col_names),
            source: InsertSource::Values(vec![col_values]),
            ignore,
            replace: is_replace,
            on_duplicate_update,
            returning,
        }));
    }

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
                    "expected VALUES, DEFAULT VALUES, SET, or SELECT in INSERT, found {:?}",
                    other,
                ),
                position: Some(p.current_pos()),
            })
        }
    };

    let on_duplicate_update = parse_on_duplicate_update_tail(p, is_replace)?;
    let returning = parse_returning_clause(p)?;
    Ok(Stmt::Insert(InsertStmt {
        table,
        columns,
        source,
        ignore,
        replace: is_replace,
        on_duplicate_update,
        returning,
    }))
}

/// Parses the optional `ON DUPLICATE KEY UPDATE col = expr, ...` tail that
/// may follow an INSERT. Returns `Ok(None)` when the next tokens are not
/// `ON DUPLICATE KEY UPDATE`.
///
/// Rejects the clause when it follows a `REPLACE INTO` — REPLACE and ODKU
/// are mutually exclusive upserts; combining them is a parse error.
fn parse_on_duplicate_update_tail(
    p: &mut Parser,
    is_replace: bool,
) -> Result<Option<Vec<Assignment>>, DbError> {
    if !matches!(p.peek(), Token::On) {
        return Ok(None);
    }
    // Two-token lookahead: only consume ON if it's "ON DUPLICATE".
    if !matches!(p.peek_at(1), Token::Ident(s) if s.eq_ignore_ascii_case("duplicate")) {
        return Ok(None);
    }
    p.advance(); // ON
    p.advance(); // DUPLICATE
                 // KEY
    match p.peek().clone() {
        Token::Key => {
            p.advance();
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("key") => {
            p.advance();
        }
        other => {
            return Err(DbError::ParseError {
                message: format!("expected KEY after ON DUPLICATE, found {other:?}"),
                position: Some(p.current_pos()),
            })
        }
    }
    p.expect(&Token::Update)?;

    if is_replace {
        return Err(DbError::ParseError {
            message: "REPLACE INTO ... ON DUPLICATE KEY UPDATE is not a valid MySQL \
                      combination (REPLACE and ODKU are mutually exclusive upserts)"
                .into(),
            position: Some(p.current_pos()),
        });
    }

    p.in_odku_assignment = true;
    let mut assignments: Vec<Assignment> = Vec::new();
    loop {
        let column = p.parse_identifier()?;
        p.expect(&Token::Eq)?;
        let value = parse_expr(p)?;
        assignments.push(Assignment { column, value });
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    p.in_odku_assignment = false;

    if assignments.is_empty() {
        return Err(DbError::ParseError {
            message: "ON DUPLICATE KEY UPDATE requires at least one assignment".into(),
            position: Some(p.current_pos()),
        });
    }
    Ok(Some(assignments))
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

fn parse_update(p: &mut Parser) -> Result<Stmt, DbError> {
    let from = parse_from_item(p)?;
    let table = match from {
        FromClause::Table(table) => table,
        FromClause::Subquery { .. }
        | FromClause::JsonTable(_)
        | FromClause::JsonbSrf(_)
        | FromClause::Values(_) => {
            return Err(DbError::ParseError {
                message: "UPDATE target must be a table".into(),
                position: Some(p.current_pos()),
            })
        }
    };
    let joins = parse_join_clauses(p)?;
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

    // `UPDATE ... ORDER BY col [ASC|DESC] LIMIT N`
    let order_by = if p.eat(&Token::Order) {
        p.expect(&Token::By)?;
        parse_order_items(p)?
    } else {
        vec![]
    };
    let limit = if p.eat(&Token::Limit) {
        Some(parse_expr(p)?)
    } else {
        None
    };

    let returning = parse_returning_clause(p)?;
    Ok(Stmt::Update(UpdateStmt {
        table,
        joins,
        assignments,
        where_clause,
        order_by,
        limit,
        returning,
    }))
}

fn parse_assignment(p: &mut Parser) -> Result<Assignment, DbError> {
    let mut column = p.parse_identifier()?;
    if p.eat(&Token::Dot) {
        column = p.parse_identifier()?;
    }
    p.expect(&Token::Eq)?;
    let value = parse_expr(p)?;
    Ok(Assignment { column, value })
}

// ── DELETE ────────────────────────────────────────────────────────────────────

fn parse_delete(p: &mut Parser) -> Result<Stmt, DbError> {
    let (target, table, joins) = if p.eat(&Token::From) {
        let from = parse_from_item(p)?;
        let table = match from {
            FromClause::Table(table) => table,
            FromClause::Subquery { .. }
            | FromClause::JsonTable(_)
            | FromClause::JsonbSrf(_)
            | FromClause::Values(_) => {
                return Err(DbError::ParseError {
                    message: "DELETE target must be a table".into(),
                    position: Some(p.current_pos()),
                })
            }
        };
        (None, table, vec![])
    } else {
        let target = p.parse_identifier()?;
        p.expect(&Token::From)?;
        let from = parse_from_item(p)?;
        let table = match from {
            FromClause::Table(table) => table,
            FromClause::Subquery { .. }
            | FromClause::JsonTable(_)
            | FromClause::JsonbSrf(_)
            | FromClause::Values(_) => {
                return Err(DbError::ParseError {
                    message: "DELETE FROM source must be a table".into(),
                    position: Some(p.current_pos()),
                })
            }
        };
        let joins = parse_join_clauses(p)?;
        (Some(target), table, joins)
    };
    let where_clause = if p.eat(&Token::Where) {
        Some(parse_expr(p)?)
    } else {
        None
    };

    // `DELETE ... ORDER BY col [ASC|DESC] LIMIT N`
    let order_by = if p.eat(&Token::Order) {
        p.expect(&Token::By)?;
        parse_order_items(p)?
    } else {
        vec![]
    };
    let limit = if p.eat(&Token::Limit) {
        Some(parse_expr(p)?)
    } else {
        None
    };

    let returning = parse_returning_clause(p)?;
    Ok(Stmt::Delete(DeleteStmt {
        table,
        target,
        joins,
        where_clause,
        order_by,
        limit,
        returning,
    }))
}
