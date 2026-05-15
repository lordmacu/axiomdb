//! DML statement parsers — SELECT, INSERT, UPDATE, DELETE.

use axiomdb_core::error::DbError;

use axiomdb_types::Value;

use crate::{
    ast::{
        Assignment, CopyFormat, CopyFromStmt, CopyOptions, CopyToStmt, DeleteStmt, FromClause,
        GroupByClause, InsertSource, InsertStmt, IntoOutfile, JoinClause, JoinCondition, JoinType,
        LockStrength, LockWaitPolicy, MergeAction, MergeActionCondition, MergeActionKind,
        MergeStmt, NullsOrder, OnConflictAction, OnConflictClause, OrderByItem, SelectHint,
        SelectItem, SelectLockClause, SelectStmt, SetOpKind, SetOpTail, SortOrder, Stmt,
        UpdateStmt,
    },
    expr::Expr,
    lexer::Token,
};

use super::{expr::parse_expr, Parser};

/// Parse a DML statement. Called by `Parser::parse_stmt`.
pub(crate) fn parse_dml(p: &mut Parser) -> Result<Stmt, DbError> {
    match p.peek() {
        // Phase 21.2 — `WITH <ctes> SELECT ...` (21.3 adds RECURSIVE).
        Token::With => {
            p.advance();
            let is_recursive = matches!(
                p.peek(),
                Token::Ident(s) if s.eq_ignore_ascii_case("RECURSIVE")
            );
            if is_recursive {
                p.advance();
            }
            let ctes = parse_cte_list(p, is_recursive)?;
            p.expect(&Token::Select)?;
            let mut s = parse_select(p)?;
            s.with_ctes = ctes;
            if matches!(p.peek(), Token::Union | Token::Intersect | Token::Except) {
                return parse_set_op(p, s);
            }
            Ok(Stmt::Select(s))
        }
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
        Token::Merge => {
            p.advance();
            parse_merge(p)
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
                "expected SELECT, INSERT, MERGE, UPDATE, or DELETE, found {:?}",
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
    let hints = if matches!(p.peek(), Token::OptimizerHint(_)) {
        parse_optimizer_hints(p)?
    } else {
        Vec::new()
    };

    let saw_distinct = p.eat(&Token::Distinct);

    // Phase 21.12 — DISTINCT ON (expr, …): key-based first-row-per-group.
    // Syntax: SELECT DISTINCT ON (e1, e2, …) …
    // Mutually exclusive with plain DISTINCT.
    let mut distinct_on: Vec<Expr> = vec![];
    let distinct = if saw_distinct && matches!(p.peek(), Token::On) {
        p.advance(); // consume ON
        p.expect(&Token::LParen)?;
        if matches!(p.peek(), Token::RParen) {
            return Err(DbError::ParseError {
                message: "DISTINCT ON requires at least one expression".into(),
                position: Some(p.current_pos()),
            });
        }
        loop {
            distinct_on.push(parse_expr(p)?);
            if !p.eat(&Token::Comma) {
                break;
            }
        }
        p.expect(&Token::RParen)?;
        false // DISTINCT ON ≠ plain DISTINCT
    } else {
        saw_distinct
    };

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

    let group_by = if p.eat(&Token::Group) {
        p.expect(&Token::By)?;
        parse_group_by_items(p)?
    } else {
        GroupByClause::None
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

    // Phase 13.7 — locking clause: FOR UPDATE/SHARE/NO KEY UPDATE/KEY SHARE [NOWAIT]
    // and MySQL alias: LOCK IN SHARE MODE
    let lock_clause = parse_lock_clause(p)?;

    // Phase 20.5b — MySQL: SELECT … INTO OUTFILE 'path' [FIELDS …] [LINES …]
    let into_outfile = parse_into_outfile(p)?;

    Ok(SelectStmt {
        with_ctes: Vec::new(),
        distinct,
        distinct_on,
        hints,
        calc_found_rows,
        columns,
        from,
        joins,
        where_clause,
        group_by,
        having,
        order_by,
        limit,
        offset,
        lock_clause,
        set_op_rest: vec![],
        into_outfile,
    })
}

/// Phase 20.5b — Parse `INTO OUTFILE 'path' [FIELDS TERMINATED BY 'x'
/// [OPTIONALLY ENCLOSED BY 'y' | ENCLOSED BY 'y']] [LINES TERMINATED BY 'z']`.
/// Returns `None` when the keyword sequence is absent.
fn parse_into_outfile(p: &mut Parser) -> Result<Option<IntoOutfile>, DbError> {
    // Lookahead: INTO OUTFILE (Ident) — do not consume if next token after INTO
    // is not the OUTFILE identifier (avoids ambiguity with other INTO uses).
    if !matches!(p.peek(), Token::Into) {
        return Ok(None);
    }
    if !matches!(p.peek_at(1), Token::Ident(s) if s.eq_ignore_ascii_case("OUTFILE")) {
        return Ok(None);
    }
    p.advance(); // consume INTO
    p.advance(); // consume OUTFILE

    let path = match p.peek().clone() {
        Token::StringLit(s) => {
            p.advance();
            s
        }
        _ => {
            return Err(DbError::ParseError {
                message: "INTO OUTFILE expects a file path string literal".into(),
                position: Some(p.current_pos()),
            })
        }
    };

    let mut field_sep: char = '\t'; // MySQL default
    let mut enclosure: Option<char> = None;
    let mut line_term: String = "\n".into(); // MySQL default

    // Parse optional FIELDS / LINES clauses in any order.
    loop {
        if p.eat_ident_ci("FIELDS") || p.eat_ident_ci("COLUMNS") {
            // TERMINATED BY
            if p.eat_ident_ci("TERMINATED") {
                p.expect(&Token::By)?;
                field_sep = parse_single_char_lit(p, "FIELDS TERMINATED BY")?;
            }
            // OPTIONALLY ENCLOSED BY or ENCLOSED BY
            if p.eat_ident_ci("OPTIONALLY") {
                p.eat_ident_ci("ENCLOSED");
                p.expect(&Token::By)?;
                enclosure = Some(parse_single_char_lit(p, "ENCLOSED BY")?);
            } else if p.eat_ident_ci("ENCLOSED") {
                p.expect(&Token::By)?;
                enclosure = Some(parse_single_char_lit(p, "ENCLOSED BY")?);
            }
            // ESCAPED BY — consume and ignore (not implemented)
            if p.eat_ident_ci("ESCAPED") {
                p.eat_ident_ci("BY");
                if matches!(p.peek(), Token::StringLit(_)) {
                    p.advance();
                }
            }
        } else if p.eat_ident_ci("LINES") {
            // STARTING BY — consume and ignore
            if p.eat_ident_ci("STARTING") {
                p.eat_ident_ci("BY");
                if matches!(p.peek(), Token::StringLit(_)) {
                    p.advance();
                }
            }
            if p.eat_ident_ci("TERMINATED") {
                p.expect(&Token::By)?;
                line_term = match p.peek().clone() {
                    Token::StringLit(s) => {
                        p.advance();
                        s
                    }
                    _ => {
                        return Err(DbError::ParseError {
                            message: "LINES TERMINATED BY expects a string literal".into(),
                            position: Some(p.current_pos()),
                        })
                    }
                };
            }
        } else {
            break;
        }
    }

    Ok(Some(IntoOutfile {
        path,
        field_sep,
        enclosure,
        line_term,
    }))
}

/// Parse a string literal that must decode to exactly one character.
fn parse_single_char_lit(p: &mut Parser, ctx: &str) -> Result<char, DbError> {
    match p.peek().clone() {
        Token::StringLit(s) => {
            p.advance();
            let mut chars = s.chars();
            let c = chars.next().ok_or_else(|| DbError::InvalidValue {
                reason: format!("{ctx} requires a single character, got empty string"),
            })?;
            if chars.next().is_some() {
                return Err(DbError::InvalidValue {
                    reason: format!("{ctx} requires a single character, got {s:?}"),
                });
            }
            Ok(c)
        }
        _ => Err(DbError::ParseError {
            message: format!("{ctx} expects a string literal"),
            position: Some(p.current_pos()),
        }),
    }
}

/// Parse `FOR UPDATE / FOR NO KEY UPDATE / FOR SHARE / FOR KEY SHARE [NOWAIT]`
/// and the MySQL alias `LOCK IN SHARE MODE`.
/// Returns `None` when no locking clause is present.
fn parse_lock_clause(p: &mut Parser) -> Result<Option<SelectLockClause>, DbError> {
    // MySQL alias: LOCK IN SHARE MODE
    if matches!(p.peek(), Token::Lock) {
        p.advance(); // LOCK
        p.expect(&Token::In)?;
        p.expect(&Token::Share)?;
        p.expect(&Token::Mode)?;
        return Ok(Some(SelectLockClause {
            strength: LockStrength::ForShare,
            wait_policy: LockWaitPolicy::Block,
        }));
    }

    if !p.eat(&Token::For) {
        return Ok(None);
    }

    // Peek at the keyword sequence after FOR.
    let strength = if p.eat(&Token::Update) {
        LockStrength::ForUpdate
    } else if matches!(p.peek(), Token::No) {
        p.advance(); // NO
        p.expect(&Token::Key)?;
        p.expect(&Token::Update)?;
        LockStrength::ForNoKeyUpdate
    } else if matches!(p.peek(), Token::Key) {
        p.advance(); // KEY
        p.expect(&Token::Share)?;
        LockStrength::ForKeyShare
    } else if matches!(p.peek(), Token::Share) {
        p.advance(); // SHARE
        LockStrength::ForShare
    } else {
        let pos = p.current_pos();
        return Err(DbError::ParseError {
            message: "expected UPDATE, SHARE, NO KEY UPDATE, or KEY SHARE after FOR".into(),
            position: Some(pos),
        });
    };

    // NOWAIT and SKIP LOCKED are not reserved keywords — use eat_ident_ci.
    let wait_policy = if eat_ident_ci(p, "NOWAIT") {
        LockWaitPolicy::NoWait
    } else if eat_ident_ci(p, "SKIP") {
        if !eat_ident_ci(p, "LOCKED") {
            return Err(DbError::ParseError {
                message: "expected LOCKED after SKIP".into(),
                position: Some(p.current_pos()),
            });
        }
        LockWaitPolicy::SkipLocked
    } else {
        LockWaitPolicy::Block
    };

    Ok(Some(SelectLockClause {
        strength,
        wait_policy,
    }))
}

fn parse_optimizer_hints(p: &mut Parser) -> Result<Vec<SelectHint>, DbError> {
    let pos = p.current_pos();
    let raw = match p.advance().token.clone() {
        Token::OptimizerHint(s) => s,
        other => {
            return Err(DbError::ParseError {
                message: format!("expected optimizer hint comment, found {other:?}"),
                position: Some(pos),
            });
        }
    };

    parse_optimizer_hint_payload(&raw, pos)
}

fn parse_optimizer_hint_payload(raw: &str, pos: usize) -> Result<Vec<SelectHint>, DbError> {
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0usize;
    let mut hints = Vec::new();

    while i < chars.len() {
        while i < chars.len() && chars[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }

        let name_start = i;
        while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
            i += 1;
        }
        if name_start == i {
            return Err(DbError::ParseError {
                message: format!("invalid optimizer hint syntax near '{}'", chars[i]),
                position: Some(pos),
            });
        }
        let name: String = chars[name_start..i].iter().collect();
        while i < chars.len() && chars[i].is_ascii_whitespace() {
            i += 1;
        }

        let arg = if i < chars.len() && chars[i] == '(' {
            i += 1;
            let arg_start = i;
            let mut depth = 1usize;
            while i < chars.len() {
                match chars[i] {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            if i >= chars.len() || chars[i] != ')' {
                return Err(DbError::ParseError {
                    message: format!("unterminated optimizer hint arguments for {name}"),
                    position: Some(pos),
                });
            }
            let payload: String = chars[arg_start..i].iter().collect();
            i += 1;
            Some(payload)
        } else {
            None
        };

        let hint = match name.to_ascii_uppercase().as_str() {
            "HASH_JOIN" => {
                if arg.is_some() {
                    return Err(DbError::ParseError {
                        message: "HASH_JOIN does not accept arguments".into(),
                        position: Some(pos),
                    });
                }
                SelectHint::HashJoin
            }
            "PARALLEL" => {
                let workers = arg
                    .ok_or_else(|| DbError::ParseError {
                        message: "PARALLEL requires a worker count".into(),
                        position: Some(pos),
                    })?
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| DbError::ParseError {
                        message: "PARALLEL requires a positive integer worker count".into(),
                        position: Some(pos),
                    })?;
                if workers == 0 {
                    return Err(DbError::ParseError {
                        message: "PARALLEL requires a positive integer worker count".into(),
                        position: Some(pos),
                    });
                }
                SelectHint::Parallel { workers }
            }
            "INDEX" => {
                let payload = arg.ok_or_else(|| DbError::ParseError {
                    message: "INDEX requires table and index arguments".into(),
                    position: Some(pos),
                })?;
                let args = split_hint_args(&payload);
                if args.len() != 2 {
                    return Err(DbError::ParseError {
                        message: "INDEX requires exactly two arguments: table and index".into(),
                        position: Some(pos),
                    });
                }
                SelectHint::Index {
                    table: args[0].clone(),
                    index: args[1].clone(),
                }
            }
            _ => {
                return Err(DbError::ParseError {
                    message: format!("unsupported optimizer hint {name}"),
                    position: Some(pos),
                });
            }
        };

        hints.push(hint);
    }

    Ok(hints)
}

fn split_hint_args(payload: &str) -> Vec<String> {
    let chars: Vec<char> = payload.chars().collect();
    let mut args = Vec::new();
    let mut i = 0usize;

    while i < chars.len() {
        while i < chars.len() && (chars[i].is_ascii_whitespace() || chars[i] == ',') {
            i += 1;
        }
        if i >= chars.len() {
            break;
        }

        if chars[i] == '`' {
            i += 1;
            let start = i;
            while i < chars.len() && chars[i] != '`' {
                i += 1;
            }
            args.push(chars[start..i.min(chars.len())].iter().collect());
            if i < chars.len() && chars[i] == '`' {
                i += 1;
            }
            continue;
        }

        let start = i;
        while i < chars.len() && !chars[i].is_ascii_whitespace() && chars[i] != ',' {
            i += 1;
        }
        args.push(chars[start..i].iter().collect());
    }

    args
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

/// Phase 21.2 — parse a comma-separated list of CTE bindings after
/// `WITH [RECURSIVE]` has been consumed. When `recursive` is true, each
/// CTE body may be `SELECT base UNION [ALL] SELECT step`.
/// Grammar per CTE: `ident [( col [, col]* )] AS ( SELECT ... )`.
fn parse_cte_list(p: &mut Parser, recursive: bool) -> Result<Vec<crate::ast::CteBinding>, DbError> {
    let mut out = Vec::new();
    loop {
        let name = p.parse_identifier()?;
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
        p.expect(&Token::As)?;
        p.expect(&Token::LParen)?;
        p.expect(&Token::Select)?;
        let body = parse_select(p)?;
        // Phase 21.3b — detect optional `UNION [ALL] SELECT step` tail
        // when the CTE is recursive. Without RECURSIVE, leave the UNION
        // for the body to consume as a SetOp later.
        let (step, union_all, is_recursive) = if recursive && matches!(p.peek(), Token::Union) {
            p.advance();
            let union_all = p.eat(&Token::All);
            p.expect(&Token::Select)?;
            let step = parse_select(p)?;
            (Some(Box::new(step)), union_all, true)
        } else {
            (None, false, false)
        };
        p.expect(&Token::RParen)?;
        out.push(crate::ast::CteBinding {
            name,
            column_names,
            query: Box::new(body),
            recursive: is_recursive,
            recursive_step: step,
            recursive_union_all: union_all,
        });
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    Ok(out)
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
    // Phase 21.9: detect LATERAL keyword before subquery.
    // Also Phase 11.20d3: accepts LATERAL before JSON_TABLE.
    let lateral_consumed = p.eat(&Token::Lateral);

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
            return parse_optional_pivot_clause(
                p,
                FromClause::Values(Box::new(crate::ast::ValuesClause {
                    rows,
                    alias,
                    column_names,
                })),
            );
        }
        p.expect(&Token::Select)?;
        let mut sub = parse_select(p)?;
        // Phase 21.9b: handle `(SELECT ... UNION [ALL] SELECT ...)` inside FROM/JOIN.
        // Fold the tails into `set_op_rest` so the executor runs them as a set operation.
        while matches!(p.peek(), Token::Union | Token::Intersect | Token::Except) {
            let kind = match p.peek() {
                Token::Union => crate::ast::SetOpKind::Union,
                Token::Intersect => crate::ast::SetOpKind::Intersect,
                Token::Except => crate::ast::SetOpKind::Except,
                _ => unreachable!(),
            };
            p.advance();
            let all = p.eat(&Token::All);
            p.expect(&Token::Select)?;
            let tail_select = parse_select(p)?;
            sub.set_op_rest.push(crate::ast::SetOpTail {
                kind,
                all,
                select: tail_select,
            });
        }
        p.expect(&Token::RParen)?;
        p.eat(&Token::As);
        let alias = p.parse_identifier()?;
        return parse_optional_pivot_clause(
            p,
            FromClause::Subquery {
                query: Box::new(sub),
                alias,
                lateral: lateral_consumed,
            },
        );
    }

    // Phase 11.20a — `JSON_TABLE(...)` table-valued function. Only dispatch
    // when followed by `(` so a user table named `json_table` without
    // arguments still parses as a plain table reference.
    if let Token::Ident(s) = p.peek() {
        if s.eq_ignore_ascii_case("JSON_TABLE") && matches!(p.peek_at(1), Token::LParen) {
            p.advance(); // consume the JSON_TABLE identifier
            let from = crate::parser::json_table::parse_json_table_call(p)?;
            return parse_optional_pivot_clause(p, from);
        }
    }

    // Phase 20.4, Step 7 — `FROM UNNEST(expr [, expr2, ...]) [AS alias(col1, col2, ...)]]`.
    // UNNEST is tokenized as Token::Unnest (not Token::Ident), so we check both.
    let peek_token = p.peek();
    let is_unnest = matches!(peek_token, Token::Unnest);
    let is_unnest_as_ident = if let Token::Ident(s) = peek_token {
        s.eq_ignore_ascii_case("UNNEST")
    } else {
        false
    };
    if (is_unnest || is_unnest_as_ident) && matches!(p.peek_at(1), Token::LParen) {
        p.advance(); // consume the UNNEST token/identifier
        p.expect(&Token::LParen)?;
        // Parse comma-separated array expressions.
        let mut exprs = vec![crate::parser::expr::parse_expr(p)?];
        while p.eat(&Token::Comma) {
            exprs.push(crate::parser::expr::parse_expr(p)?);
        }
        p.expect(&Token::RParen)?;

        // Optional alias: `AS name` or implicit `name` followed by optional `(col1, col2, ...)`.
        let alias = if p.eat(&Token::As)
            || (is_implicit_alias_token(p.peek()) && !is_pivot_clause_start(p))
        {
            Some(p.parse_identifier()?)
        } else {
            None
        };

        // Optional explicit column names: `AS u(a, b)`.
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

        // Validate that column_names width matches exprs width if both are present.
        if let (Some(ref cols), n) = (&column_names, exprs.len()) {
            if cols.len() != n {
                return Err(DbError::ParseError {
                    message: format!(
                        "UNNEST: {} column names given but {} array expressions present",
                        cols.len(),
                        n
                    ),
                    position: Some(p.current_pos()),
                });
            }
        }

        return parse_optional_pivot_clause(
            p,
            FromClause::Unnest(Box::new(crate::ast::UnnestClause {
                exprs,
                alias,
                column_names: column_names.unwrap_or_default(),
                lateral: lateral_consumed,
            })),
        );
    }

    // Phase 20.10 — `FROM GENERATE_SERIES(start, stop [, step]) [AS alias(col)]`.
    if let Token::Ident(s) = p.peek() {
        if s.eq_ignore_ascii_case("GENERATE_SERIES") && matches!(p.peek_at(1), Token::LParen) {
            p.advance(); // consume identifier
            p.expect(&Token::LParen)?;
            let start = crate::parser::expr::parse_expr(p)?;
            p.expect(&Token::Comma)?;
            let stop = crate::parser::expr::parse_expr(p)?;
            let step = if p.eat(&Token::Comma) {
                Some(crate::parser::expr::parse_expr(p)?)
            } else {
                None
            };
            p.expect(&Token::RParen)?;

            let alias = if p.eat(&Token::As)
                || (is_implicit_alias_token(p.peek()) && !is_pivot_clause_start(p))
            {
                Some(p.parse_identifier()?)
            } else {
                None
            };

            let column_name = if p.eat(&Token::LParen) {
                let col = p.parse_identifier()?;
                p.expect(&Token::RParen)?;
                Some(col)
            } else {
                None
            };

            return parse_optional_pivot_clause(
                p,
                FromClause::GenerateSeries(Box::new(crate::ast::GenerateSeriesClause {
                    start,
                    stop,
                    step,
                    alias,
                    column_name,
                })),
            );
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
                let alias = if p.eat(&Token::As)
                    || (is_implicit_alias_token(p.peek()) && !is_pivot_clause_start(p))
                {
                    Some(p.parse_identifier()?)
                } else {
                    None
                };
                return parse_optional_pivot_clause(
                    p,
                    FromClause::JsonbSrf(Box::new(crate::ast::JsonbSrf { kind, doc, alias })),
                );
            }
        }
    }

    // Regular table reference
    let mut table_ref = p.parse_table_ref()?;

    // Optional alias: `AS name` or implicit `name` (if next is a plain identifier,
    // not a keyword like JOIN, WHERE, ON, etc.)
    if p.eat(&Token::As) || (is_implicit_alias_token(p.peek()) && !is_pivot_clause_start(p)) {
        table_ref.alias = Some(p.parse_identifier()?);
    }

    parse_optional_pivot_clause(p, FromClause::Table(table_ref))
}

fn parse_optional_pivot_clause(p: &mut Parser, source: FromClause) -> Result<FromClause, DbError> {
    if !is_pivot_clause_start(p) {
        return Ok(source);
    }

    p.advance(); // PIVOT
    p.expect(&Token::LParen)?;

    let (aggregate_name, aggregate_arg) = match parse_expr(p)? {
        Expr::Function { name, args } if args.len() == 1 => {
            (name, args.into_iter().next().unwrap())
        }
        Expr::Function { name, args } => {
            return Err(DbError::ParseError {
                message: format!(
                    "PIVOT aggregate `{name}` must take exactly one argument, found {}",
                    args.len()
                ),
                position: Some(p.current_pos()),
            });
        }
        other => {
            return Err(DbError::ParseError {
                message: format!(
                    "PIVOT requires an aggregate function call, found {:?}",
                    other
                ),
                position: Some(p.current_pos()),
            });
        }
    };

    if !p.eat(&Token::For) {
        return Err(DbError::ParseError {
            message: "expected FOR in PIVOT clause".into(),
            position: Some(p.current_pos()),
        });
    }
    let (pivot_expr, values) = match parse_expr(p)? {
        Expr::In {
            expr,
            list,
            negated: false,
        } => {
            if !list.iter().all(|value| matches!(value, Expr::Literal(_))) {
                return Err(DbError::ParseError {
                    message: "PIVOT IN values must be literals".into(),
                    position: Some(p.current_pos()),
                });
            }
            (*expr, list)
        }
        _ => {
            return Err(DbError::ParseError {
                message: "expected IN (...) in PIVOT clause".into(),
                position: Some(p.current_pos()),
            });
        }
    };
    p.expect(&Token::RParen)?;

    let alias = if p.eat(&Token::As) || is_implicit_alias_token(p.peek()) {
        Some(p.parse_identifier()?)
    } else {
        None
    };

    Ok(FromClause::Pivot(Box::new(crate::ast::PivotClause {
        source: Box::new(source),
        aggregate_name,
        aggregate_arg,
        pivot_expr,
        values,
        alias,
    })))
}

fn is_pivot_clause_start(p: &Parser) -> bool {
    peek_ident_ci_at(p, 0, "PIVOT") && p.peek_at(1) == &Token::LParen
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

    // Phase 21.9: Handle comma-separated tables as implicit CROSS JOINs.
    // This allows `FROM t, LATERAL (SELECT ...) sub` syntax.
    loop {
        if !matches!(p.peek(), Token::Comma) {
            break;
        }
        p.advance();
        let table = parse_from_item(p)?;
        joins.push(JoinClause {
            join_type: JoinType::Cross,
            table,
            condition: JoinCondition::On(Expr::Literal(Value::Bool(true))),
            natural: false,
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

/// Parse a single argument to `CALL proc(arg1, arg2, ...)`.
/// Exposed as `pub(crate)` for use by the top-level parser dispatch.
pub(crate) fn parse_call_arg(p: &mut Parser) -> Result<Expr, DbError> {
    parse_expr(p)
}

// ── GROUP BY clause parser (Phase 21.21) ─────────────────────────────────────

/// Find or insert `expr` in the universe list (deduplication).
/// Returns the index of the expression in the universe.
fn grouping_intern(universe: &mut Vec<Expr>, expr: Expr) -> usize {
    if let Some(pos) = universe.iter().position(|e| e == &expr) {
        pos
    } else {
        let idx = universe.len();
        universe.push(expr);
        idx
    }
}

/// Check whether the next token is an identifier matching `keyword` (case-insensitive).
/// Does not advance the parser.
fn peek_ident_ci_at(p: &Parser, offset: usize, keyword: &str) -> bool {
    match p.peek_at(offset) {
        Token::Ident(s) => s.eq_ignore_ascii_case(keyword),
        _ => false,
    }
}

/// Parse the argument list inside `ROLLUP(...)` or `CUBE(...)`.
///
/// Each argument can be:
///  - A simple expression: `a`  → contributes one universe slot
///  - A tuple `(a, b)` → contributes multiple slots treated as a composite element
///
/// Returns indices into `universe` (one per composite element; tuples contribute
/// one index per column inside the composite, all added to the result flat list).
fn parse_grouping_arg_list(
    p: &mut Parser,
    universe: &mut Vec<Expr>,
) -> Result<Vec<usize>, DbError> {
    let mut items: Vec<usize> = Vec::new();
    loop {
        if p.peek() == &Token::LParen {
            p.advance(); // consume '('
                         // Tuple of exprs — add each as a separate universe slot.
            loop {
                let e = parse_expr(p)?;
                items.push(grouping_intern(universe, e));
                if p.peek() == &Token::RParen {
                    break;
                }
                p.expect(&Token::Comma)?;
            }
            p.expect(&Token::RParen)?;
        } else {
            let e = parse_expr(p)?;
            items.push(grouping_intern(universe, e));
        }
        if p.peek() != &Token::Comma {
            break;
        }
        p.advance(); // consume ','
                     // Stop if we see the closing ')' of the outer ROLLUP/CUBE call.
        if p.peek() == &Token::RParen {
            break;
        }
    }
    Ok(items)
}

/// Parse the content of `GROUPING SETS(...)`.
///
/// Grammar:
///   grouping_sets_list ::= grouping_sets_item (',' grouping_sets_item)*
///   grouping_sets_item ::= '(' expr_list_or_empty ')'
///                        | ROLLUP '(' grouping_args ')'
///                        | CUBE '(' grouping_args ')'
///                        | expr
///
/// Nested ROLLUP/CUBE inside GROUPING SETS is flattened (PostgreSQL semantics).
/// Nested GROUPING SETS is also flattened.
fn parse_grouping_sets_content(
    p: &mut Parser,
    universe: &mut Vec<Expr>,
) -> Result<Vec<Vec<usize>>, DbError> {
    let mut sets: Vec<Vec<usize>> = Vec::new();
    loop {
        if peek_ident_ci_at(p, 0, "ROLLUP") && p.peek_at(1) == &Token::LParen {
            eat_ident_ci(p, "ROLLUP");
            p.expect(&Token::LParen)?;
            let items = parse_grouping_arg_list(p, universe)?;
            p.expect(&Token::RParen)?;
            // ROLLUP(items) → N+1 sets (full prefix down to empty)
            let n = items.len();
            for k in (0..=n).rev() {
                sets.push(items[..k].to_vec());
            }
        } else if peek_ident_ci_at(p, 0, "CUBE") && p.peek_at(1) == &Token::LParen {
            eat_ident_ci(p, "CUBE");
            p.expect(&Token::LParen)?;
            let items = parse_grouping_arg_list(p, universe)?;
            p.expect(&Token::RParen)?;
            let n = items.len();
            if n > 16 {
                return Err(DbError::ParseError {
                    message: format!(
                        "CUBE with {} dimensions would produce {} sets (maximum is 65536)",
                        n,
                        1usize << n
                    ),
                    position: Some(p.current_pos()),
                });
            }
            let total = 1usize << n;
            let mut cube_sets: Vec<Vec<usize>> = (0..total)
                .map(|mask| {
                    (0..n)
                        .filter(|&i| (mask >> i) & 1 == 1)
                        .map(|i| items[i])
                        .collect()
                })
                .collect();
            cube_sets.sort_by_key(|s| std::cmp::Reverse(s.len()));
            sets.extend(cube_sets);
        } else if peek_ident_ci_at(p, 0, "GROUPING")
            && peek_ident_ci_at(p, 1, "SETS")
            && p.peek_at(2) == &Token::LParen
        {
            // Nested GROUPING SETS — flatten by appending its sets directly.
            eat_ident_ci(p, "GROUPING");
            eat_ident_ci(p, "SETS");
            p.expect(&Token::LParen)?;
            let inner = parse_grouping_sets_content(p, universe)?;
            p.expect(&Token::RParen)?;
            sets.extend(inner);
        } else if p.peek() == &Token::LParen {
            p.advance(); // '('
            if p.peek() == &Token::RParen {
                // Empty set () → grand total
                p.advance(); // ')'
                sets.push(vec![]);
            } else {
                // Explicit set of exprs: (a, b, ...)
                let mut set_items = Vec::new();
                loop {
                    let e = parse_expr(p)?;
                    set_items.push(grouping_intern(universe, e));
                    if p.peek() == &Token::RParen {
                        break;
                    }
                    p.expect(&Token::Comma)?;
                }
                p.expect(&Token::RParen)?;
                sets.push(set_items);
            }
        } else {
            // Single bare expression — treated as a singleton set
            let e = parse_expr(p)?;
            let idx = grouping_intern(universe, e);
            sets.push(vec![idx]);
        }

        if p.peek() != &Token::Comma {
            break;
        }
        p.advance(); // consume ','
                     // Stop if we've hit the closing ')' of the outer GROUPING SETS call
        if p.peek() == &Token::RParen {
            break;
        }
    }
    Ok(sets)
}

/// Parse a full `GROUP BY` clause (after `GROUP BY` keywords have been consumed).
///
/// Handles:
/// - Plain `GROUP BY a, b, c`          → `GroupByClause::Simple`
/// - MySQL `GROUP BY a, b WITH ROLLUP`  → `GroupByClause::WithRollup`
/// - Standard `GROUP BY ROLLUP(a, b)`   → `GroupByClause::Sets`
/// - Standard `GROUP BY CUBE(a, b)`     → `GroupByClause::Sets`
/// - Standard `GROUP BY GROUPING SETS(...)` → `GroupByClause::Sets`
/// - Mixed `GROUP BY a, ROLLUP(b, c)`   → `GroupByClause::Sets` (cross-product)
fn parse_group_by_items(p: &mut Parser) -> Result<GroupByClause, DbError> {
    let mut universe: Vec<Expr> = Vec::new();
    // Each entry is one GROUP BY item's contribution as a list of grouping sets.
    // A plain expr `a` contributes `[[idx_a]]` (one set containing one index).
    // ROLLUP(a,b) contributes `[[a,b],[a],[]]`.
    let mut item_sets: Vec<Vec<Vec<usize>>> = Vec::new();
    let mut has_special = false;

    loop {
        if peek_ident_ci_at(p, 0, "ROLLUP") && p.peek_at(1) == &Token::LParen {
            has_special = true;
            eat_ident_ci(p, "ROLLUP");
            p.expect(&Token::LParen)?;
            let items = parse_grouping_arg_list(p, &mut universe)?;
            p.expect(&Token::RParen)?;
            if items.is_empty() {
                return Err(DbError::ParseError {
                    message: "ROLLUP requires at least one expression".into(),
                    position: Some(p.current_pos()),
                });
            }
            let n = items.len();
            let mut sets: Vec<Vec<usize>> = Vec::new();
            for k in (0..=n).rev() {
                sets.push(items[..k].to_vec());
            }
            item_sets.push(sets);
        } else if peek_ident_ci_at(p, 0, "CUBE") && p.peek_at(1) == &Token::LParen {
            has_special = true;
            eat_ident_ci(p, "CUBE");
            p.expect(&Token::LParen)?;
            let items = parse_grouping_arg_list(p, &mut universe)?;
            p.expect(&Token::RParen)?;
            if items.is_empty() {
                return Err(DbError::ParseError {
                    message: "CUBE requires at least one expression".into(),
                    position: Some(p.current_pos()),
                });
            }
            let n = items.len();
            if n > 16 {
                return Err(DbError::ParseError {
                    message: format!(
                        "CUBE with {} dimensions would produce {} sets (maximum is 65536)",
                        n,
                        1usize << n
                    ),
                    position: Some(p.current_pos()),
                });
            }
            let total = 1usize << n;
            let mut cube_sets: Vec<Vec<usize>> = (0..total)
                .map(|mask| {
                    (0..n)
                        .filter(|&i| (mask >> i) & 1 == 1)
                        .map(|i| items[i])
                        .collect()
                })
                .collect();
            cube_sets.sort_by_key(|s| std::cmp::Reverse(s.len()));
            item_sets.push(cube_sets);
        } else if peek_ident_ci_at(p, 0, "GROUPING")
            && peek_ident_ci_at(p, 1, "SETS")
            && p.peek_at(2) == &Token::LParen
        {
            has_special = true;
            eat_ident_ci(p, "GROUPING");
            eat_ident_ci(p, "SETS");
            p.expect(&Token::LParen)?;
            let sets = parse_grouping_sets_content(p, &mut universe)?;
            p.expect(&Token::RParen)?;
            if sets.is_empty() {
                return Err(DbError::ParseError {
                    message: "GROUPING SETS requires at least one grouping set".into(),
                    position: Some(p.current_pos()),
                });
            }
            item_sets.push(sets);
        } else {
            // Plain expression — one set containing one index.
            let e = parse_expr(p)?;
            let idx = grouping_intern(&mut universe, e);
            item_sets.push(vec![vec![idx]]);
        }

        if p.peek() == &Token::Comma {
            p.advance(); // consume ','
        } else {
            break;
        }
    }

    if !has_special {
        // Check for MySQL-style WITH ROLLUP.
        if matches!(p.peek(), Token::With) && peek_ident_ci_at(p, 1, "rollup") {
            p.advance(); // WITH
            p.advance(); // ROLLUP
            return Ok(GroupByClause::WithRollup(universe));
        }
        return Ok(GroupByClause::Simple(universe));
    }

    // Cross-product all item_sets into a flat list of grouping sets.
    let mut result: Vec<Vec<usize>> = vec![vec![]];
    for item in item_sets {
        let mut new_result: Vec<Vec<usize>> = Vec::new();
        for existing in &result {
            for set in &item {
                let mut combined: Vec<usize> = existing.clone();
                for &idx in set {
                    if !combined.contains(&idx) {
                        combined.push(idx);
                    }
                }
                combined.sort_unstable();
                new_result.push(combined);
            }
        }
        result = new_result;
    }

    let total_sets = result.len();
    if total_sets > 65535 {
        return Err(DbError::ParseError {
            message: format!("Grouping set count {} exceeds maximum 65535", total_sets),
            position: Some(p.current_pos()),
        });
    }

    Ok(GroupByClause::Sets {
        universe,
        sets: result,
    })
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
        let on_conflict = parse_on_conflict_tail(p, is_replace)?;
        let returning = parse_returning_clause(p)?;
        return Ok(Stmt::Insert(InsertStmt {
            table,
            columns: Some(col_names),
            source: InsertSource::Values(vec![col_values]),
            ignore,
            replace: is_replace,
            on_duplicate_update,
            on_conflict,
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
    let on_conflict = parse_on_conflict_tail(p, is_replace)?;
    let returning = parse_returning_clause(p)?;
    Ok(Stmt::Insert(InsertStmt {
        table,
        columns,
        source,
        ignore,
        replace: is_replace,
        on_duplicate_update,
        on_conflict,
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

/// Parses PostgreSQL `ON CONFLICT ...` after an INSERT source.
fn parse_on_conflict_tail(
    p: &mut Parser,
    is_replace: bool,
) -> Result<Option<OnConflictClause>, DbError> {
    if !matches!(p.peek(), Token::On) {
        return Ok(None);
    }
    if !matches!(p.peek_at(1), Token::Ident(s) if s.eq_ignore_ascii_case("conflict")) {
        return Ok(None);
    }
    p.advance(); // ON
    p.advance(); // CONFLICT

    if is_replace {
        return Err(DbError::ParseError {
            message: "REPLACE INTO ... ON CONFLICT is not a valid combination \
                      (REPLACE and ON CONFLICT are mutually exclusive upserts)"
                .into(),
            position: Some(p.current_pos()),
        });
    }

    let target_columns = if p.eat(&Token::LParen) {
        let mut cols = vec![p.parse_identifier()?];
        while p.eat(&Token::Comma) {
            cols.push(p.parse_identifier()?);
        }
        p.expect(&Token::RParen)?;
        cols
    } else {
        Vec::new()
    };

    p.expect(&Token::Do)?;
    let action = if eat_ident_ci(p, "nothing") {
        OnConflictAction::DoNothing
    } else if p.eat(&Token::Update) {
        if target_columns.is_empty() {
            return Err(DbError::ParseError {
                message: "ON CONFLICT DO UPDATE requires a conflict target".into(),
                position: Some(p.current_pos()),
            });
        }
        p.expect(&Token::Set)?;

        let old_flag = p.in_on_conflict_expr;
        p.in_on_conflict_expr = true;
        let parsed = (|| -> Result<OnConflictAction, DbError> {
            let mut assignments = vec![parse_assignment(p)?];
            while p.eat(&Token::Comma) {
                assignments.push(parse_assignment(p)?);
            }
            let where_clause = if p.eat(&Token::Where) {
                Some(parse_expr(p)?)
            } else {
                None
            };
            Ok(OnConflictAction::DoUpdate {
                assignments,
                where_clause,
            })
        })();
        p.in_on_conflict_expr = old_flag;
        parsed?
    } else {
        return Err(DbError::ParseError {
            message: "expected NOTHING or UPDATE after ON CONFLICT DO".into(),
            position: Some(p.current_pos()),
        });
    };

    Ok(Some(OnConflictClause {
        target_columns,
        action,
    }))
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

fn parse_update(p: &mut Parser) -> Result<Stmt, DbError> {
    let from = parse_from_item(p)?;
    let table = match from {
        FromClause::Table(table) => table,
        FromClause::Subquery { .. }
        | FromClause::JsonTable(_)
        | FromClause::JsonbSrf(_)
        | FromClause::Values(_)
        | FromClause::RecursiveCte(_)
        | FromClause::Pivot(_)
        | FromClause::Unnest(_)
        | FromClause::GenerateSeries(_) => {
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

// ── MERGE ────────────────────────────────────────────────────────────────────

fn parse_merge(p: &mut Parser) -> Result<Stmt, DbError> {
    p.expect(&Token::Into)?;
    let mut target = p.parse_table_ref()?;
    if p.eat(&Token::As) || is_implicit_alias_token(p.peek()) {
        target.alias = Some(p.parse_identifier()?);
    }

    p.expect(&Token::Using)?;
    let source = parse_from_item(p)?;

    p.expect(&Token::On)?;
    let on = parse_expr(p)?;

    let mut actions = Vec::new();
    while p.eat(&Token::When) {
        actions.push(parse_merge_action(p)?);
    }
    if actions.is_empty() {
        return Err(DbError::ParseError {
            message: "MERGE requires at least one WHEN branch".into(),
            position: Some(p.current_pos()),
        });
    }

    Ok(Stmt::Merge(MergeStmt {
        target,
        source,
        on,
        actions,
    }))
}

fn parse_merge_action(p: &mut Parser) -> Result<MergeAction, DbError> {
    let condition = if p.eat(&Token::Not) {
        expect_ident_ci(p, "MATCHED")?;
        if p.eat(&Token::By) {
            if p.eat_ident_ci("SOURCE") {
                return Err(DbError::NotImplemented {
                    feature: "MERGE WHEN NOT MATCHED BY SOURCE".into(),
                });
            }
            expect_ident_ci(p, "TARGET")?;
        }
        MergeActionCondition::NotMatched
    } else {
        expect_ident_ci(p, "MATCHED")?;
        MergeActionCondition::Matched
    };

    let guard = if p.eat(&Token::And) {
        Some(parse_expr(p)?)
    } else {
        None
    };

    p.expect(&Token::Then)?;
    let kind = parse_merge_action_kind(p, condition)?;
    Ok(MergeAction {
        condition,
        guard,
        kind,
    })
}

fn parse_merge_action_kind(
    p: &mut Parser,
    condition: MergeActionCondition,
) -> Result<MergeActionKind, DbError> {
    if p.eat(&Token::Update) {
        if condition != MergeActionCondition::Matched {
            return Err(DbError::ParseError {
                message: "MERGE UPDATE action requires WHEN MATCHED".into(),
                position: Some(p.current_pos()),
            });
        }
        p.expect(&Token::Set)?;
        let mut assignments = vec![parse_assignment(p)?];
        while p.eat(&Token::Comma) {
            assignments.push(parse_assignment(p)?);
        }
        return Ok(MergeActionKind::Update(assignments));
    }

    if p.eat(&Token::Delete) {
        if condition != MergeActionCondition::Matched {
            return Err(DbError::ParseError {
                message: "MERGE DELETE action requires WHEN MATCHED".into(),
                position: Some(p.current_pos()),
            });
        }
        return Ok(MergeActionKind::Delete);
    }

    if p.eat(&Token::Do) {
        expect_ident_ci(p, "NOTHING")?;
        return Ok(MergeActionKind::DoNothing);
    }

    if p.eat(&Token::Insert) {
        if condition != MergeActionCondition::NotMatched {
            return Err(DbError::ParseError {
                message: "MERGE INSERT action requires WHEN NOT MATCHED".into(),
                position: Some(p.current_pos()),
            });
        }
        let columns = if p.eat(&Token::LParen) {
            let mut cols = vec![p.parse_identifier()?];
            while p.eat(&Token::Comma) {
                cols.push(p.parse_identifier()?);
            }
            p.expect(&Token::RParen)?;
            Some(cols)
        } else {
            None
        };
        p.expect(&Token::Values)?;
        p.expect(&Token::LParen)?;
        let mut values = vec![parse_expr(p)?];
        while p.eat(&Token::Comma) {
            values.push(parse_expr(p)?);
        }
        p.expect(&Token::RParen)?;
        return Ok(MergeActionKind::Insert { columns, values });
    }

    Err(DbError::ParseError {
        message: format!(
            "expected UPDATE, DELETE, INSERT, or DO NOTHING after MERGE THEN, found {:?}",
            p.peek(),
        ),
        position: Some(p.current_pos()),
    })
}

fn expect_ident_ci(p: &mut Parser, keyword: &str) -> Result<(), DbError> {
    if p.eat_ident_ci(keyword) {
        Ok(())
    } else {
        Err(DbError::ParseError {
            message: format!("expected {keyword}, found {:?}", p.peek()),
            position: Some(p.current_pos()),
        })
    }
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
            | FromClause::Values(_)
            | FromClause::RecursiveCte(_)
            | FromClause::Pivot(_)
            | FromClause::Unnest(_)
            | FromClause::GenerateSeries(_) => {
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
            | FromClause::Values(_)
            | FromClause::RecursiveCte(_)
            | FromClause::Pivot(_)
            | FromClause::Unnest(_)
            | FromClause::GenerateSeries(_) => {
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

// ── COPY FROM / TO ────────────────────────────────────────────────────────────

/// Parse `COPY table FROM|TO 'path' [WITH ( options )]`.
///
/// Called after `COPY` has been consumed by the top-level dispatcher.
pub(crate) fn parse_copy(p: &mut Parser) -> Result<Stmt, DbError> {
    let table = p.parse_identifier()?;

    let is_from = if p.eat(&Token::From) {
        true
    } else if p.eat(&Token::To) {
        false
    } else {
        return Err(DbError::ParseError {
            message: format!("expected FROM or TO after COPY table, found {:?}", p.peek()),
            position: Some(p.current_pos()),
        });
    };

    // path — single-quoted string literal
    let path = match p.peek().clone() {
        Token::StringLit(s) => {
            p.advance();
            s
        }
        other => {
            return Err(DbError::ParseError {
                message: format!(
                    "expected file path string after COPY … FROM/TO, found {:?}",
                    other
                ),
                position: Some(p.current_pos()),
            });
        }
    };

    // Optional WITH ( options )
    let options = if p.eat(&Token::With) {
        p.expect(&Token::LParen)?;
        let opts = parse_copy_options(p)?;
        p.expect(&Token::RParen)?;
        opts
    } else {
        CopyOptions::default()
    };

    if is_from {
        Ok(Stmt::CopyFrom(CopyFromStmt {
            table,
            path,
            options,
        }))
    } else {
        Ok(Stmt::CopyTo(CopyToStmt {
            table,
            path,
            options,
        }))
    }
}

fn parse_copy_options(p: &mut Parser) -> Result<CopyOptions, DbError> {
    let mut opts = CopyOptions::default();
    loop {
        if p.peek() == &Token::RParen {
            break;
        }
        // Consume option key — handle both identifier tokens and reserved keywords
        // that happen to be valid COPY option names (e.g. Token::Null).
        let key = match p.peek().clone() {
            Token::Null => {
                p.advance();
                "NULL".to_string()
            }
            _ => p.parse_identifier()?,
        };
        match key.to_ascii_uppercase().as_str() {
            "FORMAT" => {
                // FORMAT value: JSON lexes as Token::TyJson, not Token::Ident.
                let fmt_str = match p.peek().clone() {
                    Token::TyJson => {
                        p.advance();
                        "JSON".to_string()
                    }
                    _ => p.parse_identifier()?,
                };
                opts.format = Some(match fmt_str.to_ascii_uppercase().as_str() {
                    "CSV" => CopyFormat::Csv,
                    "JSON" => CopyFormat::Json,
                    "JSONL" | "NDJSON" => CopyFormat::Jsonl,
                    other => {
                        return Err(DbError::ParseError {
                            message: format!(
                                "unknown COPY FORMAT '{}'; expected CSV, JSON, or JSONL",
                                other
                            ),
                            position: Some(p.current_pos()),
                        });
                    }
                });
            }
            "HEADER" => {
                opts.header = Some(match p.peek().clone() {
                    Token::True => {
                        p.advance();
                        true
                    }
                    Token::False => {
                        p.advance();
                        false
                    }
                    Token::Ident(s) if s.eq_ignore_ascii_case("true") => {
                        p.advance();
                        true
                    }
                    Token::Ident(s) if s.eq_ignore_ascii_case("false") => {
                        p.advance();
                        false
                    }
                    // bare HEADER without value = TRUE (PostgreSQL compat)
                    _ => true,
                });
            }
            "DELIMITER" => {
                let delim_str = match p.peek().clone() {
                    Token::StringLit(s) => {
                        p.advance();
                        s
                    }
                    other => {
                        return Err(DbError::ParseError {
                            message: format!(
                                "COPY DELIMITER must be a single-character string, found {:?}",
                                other
                            ),
                            position: Some(p.current_pos()),
                        });
                    }
                };
                let mut chars = delim_str.chars();
                let ch = chars.next().ok_or_else(|| DbError::ParseError {
                    message: "COPY DELIMITER must be a single character".into(),
                    position: Some(p.current_pos()),
                })?;
                if chars.next().is_some() {
                    return Err(DbError::ParseError {
                        message: "COPY DELIMITER must be a single character".into(),
                        position: Some(p.current_pos()),
                    });
                }
                opts.delimiter = Some(ch);
            }
            "NULL" => {
                opts.null_str = Some(match p.peek().clone() {
                    Token::StringLit(s) => {
                        p.advance();
                        s
                    }
                    other => {
                        return Err(DbError::ParseError {
                            message: format!(
                                "COPY NULL must be a string literal, found {:?}",
                                other
                            ),
                            position: Some(p.current_pos()),
                        });
                    }
                });
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!(
                        "unknown COPY option '{}'; expected FORMAT, HEADER, DELIMITER, or NULL",
                        other
                    ),
                    position: Some(p.current_pos()),
                });
            }
        }
        // Optional comma between options
        p.eat(&Token::Comma);
    }
    Ok(opts)
}
