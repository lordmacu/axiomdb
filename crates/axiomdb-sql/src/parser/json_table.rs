//! Phase 11.20a — parser for `JSON_TABLE(doc, '$.path' COLUMNS (...))`.
//!
//! Dispatched from `parse_from_item` when the identifier `JSON_TABLE`
//! (case-insensitive) is followed by `(`. Rollback-safe: if the caller
//! wants to parse a table literally named `json_table`, use without `(`.

use axiomdb_core::error::DbError;

use crate::ast::{FromClause, JsonTable, JsonTableColumn};
use crate::expr::{SqlJsonOnBehavior, SqlJsonQuotes, SqlJsonWrapper};
use crate::lexer::Token;
use crate::parser::ddl::parse_data_type;
use crate::parser::expr::parse_expr;
use crate::parser::sql_json_common::{
    parse_optional_passing, parse_optional_quotes, parse_optional_wrapper,
};
use crate::parser::Parser;

/// Parse a `JSON_TABLE(...)` call, assuming the `JSON_TABLE` identifier and
/// the opening `(` have NOT yet been consumed.
///
/// Returns the fully constructed `FromClause::JsonTable(..)` including
/// optional trailing alias.
pub(crate) fn parse_json_table_call(p: &mut Parser<'_>) -> Result<FromClause, DbError> {
    // JSON_TABLE already consumed by the caller — we expect `(` next.
    p.expect(&Token::LParen)?;
    let doc = parse_expr(p)?;
    p.expect(&Token::Comma)?;
    let row_path = p.parse_string_literal()?;
    // Optional `PASSING expr AS name [, …]` between the row path and
    // `COLUMNS(...)` — Phase 11.20d1. Bindings are visible to the row path
    // AND every column / NESTED path.
    let passing = parse_optional_passing(p)?;
    // Reject duplicate variable names up-front.
    {
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        for (_, name) in &passing {
            if !seen.insert(name.to_ascii_lowercase()) {
                return Err(DbError::ParseError {
                    message: format!("JSON_TABLE: duplicate PASSING variable `{name}`"),
                    position: Some(p.current_pos()),
                });
            }
        }
    }
    // Require `COLUMNS` keyword (not currently a reserved token → ident-ci).
    if !p.eat_ident_ci("COLUMNS") {
        return Err(DbError::ParseError {
            message: "JSON_TABLE: expected COLUMNS(...) clause".into(),
            position: Some(p.current_pos()),
        });
    }
    p.expect(&Token::LParen)?;
    let mut columns = Vec::new();
    loop {
        columns.push(parse_column_def(p)?);
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    p.expect(&Token::RParen)?;
    p.expect(&Token::RParen)?;

    // Optional alias: `AS name` or implicit plain identifier.
    let alias = if p.eat(&Token::As)
        || matches!(
            p.peek(),
            Token::Ident(_) | Token::QuotedIdent(_) | Token::DqIdent(_)
        ) {
        Some(p.parse_identifier()?)
    } else {
        None
    };

    Ok(FromClause::JsonTable(Box::new(JsonTable {
        doc,
        row_path,
        passing,
        columns,
        alias,
    })))
}

fn parse_column_def(p: &mut Parser<'_>) -> Result<JsonTableColumn, DbError> {
    // `NESTED [PATH] '<jsonpath>' COLUMNS (...)` — Phase 11.20b.
    // Dispatch before the generic identifier path since `NESTED` is not a
    // reserved token but, when it appears as the first token of a column
    // spec, it introduces a nested shred rather than a column name.
    if p.eat_ident_ci("NESTED") {
        // `PATH` keyword is optional (SQL:2016 shortcut + MariaDB parity).
        let _ = p.eat_ident_ci("PATH");
        let path = p.parse_string_literal()?;
        if !p.eat_ident_ci("COLUMNS") {
            return Err(DbError::ParseError {
                message: "JSON_TABLE NESTED: expected COLUMNS(...) clause".into(),
                position: Some(p.current_pos()),
            });
        }
        p.expect(&Token::LParen)?;
        let mut columns = Vec::new();
        loop {
            columns.push(parse_column_def(p)?);
            if !p.eat(&Token::Comma) {
                break;
            }
        }
        p.expect(&Token::RParen)?;
        return Ok(JsonTableColumn::Nested { path, columns });
    }

    let name = p.parse_identifier()?;

    // `name FOR ORDINALITY` — dispatch first (`FOR` is a reserved token).
    if p.eat(&Token::For) {
        if !p.eat_ident_ci("ORDINALITY") {
            return Err(DbError::ParseError {
                message: format!("JSON_TABLE column `{name}`: expected ORDINALITY after FOR"),
                position: Some(p.current_pos()),
            });
        }
        return Ok(JsonTableColumn::Ordinality { name });
    }

    // `name TYPE [ EXISTS ] PATH '...' [on_empty] [on_error]`.
    let parsed = parse_data_type(p)?;
    let ty = parsed.data_type;
    // `EXISTS` is a reserved token (used in `EXISTS(SELECT …)`); `PATH` is
    // not reserved — it's a plain identifier.
    let is_exists = p.eat(&Token::Exists);

    if !p.eat_ident_ci("PATH") {
        return Err(DbError::ParseError {
            message: format!("JSON_TABLE column `{name}`: expected PATH '<jsonpath>'"),
            position: Some(p.current_pos()),
        });
    }
    let path = p.parse_string_literal()?;

    if is_exists {
        let on_error = parse_optional_exists_on_error(p)?;
        return Ok(JsonTableColumn::Exists {
            name,
            ty,
            path,
            on_error,
        });
    }

    // Regular column clause order — SQL:2016 / PG / Oracle:
    //   PATH  →  WRAPPER  →  QUOTES  →  ON EMPTY  →  ON ERROR.
    let wrapper = parse_optional_wrapper(p)?.unwrap_or(SqlJsonWrapper::Without);
    let quotes = parse_optional_quotes(p)?.unwrap_or(SqlJsonQuotes::Keep);
    // `OMIT QUOTES` is only meaningful on TEXT-returning columns
    // (SQL:2016 §9.42 + PG parity). Reject early.
    if matches!(quotes, SqlJsonQuotes::Omit) && !matches!(ty, axiomdb_types::DataType::Text) {
        return Err(DbError::ParseError {
            message: format!(
                "JSON_TABLE column `{name}`: OMIT QUOTES is only valid on TEXT-returning columns"
            ),
            position: Some(p.current_pos()),
        });
    }
    let (on_empty, on_error) = parse_on_empty_on_error(p)?;
    Ok(JsonTableColumn::Regular {
        name,
        ty,
        path,
        wrapper,
        quotes,
        on_empty,
        on_error,
    })
}

/// Accepts `[ {NULL | ERROR | DEFAULT expr} ON EMPTY ] [ {NULL | ERROR | DEFAULT expr} ON ERROR ]`.
fn parse_on_empty_on_error(
    p: &mut Parser<'_>,
) -> Result<(SqlJsonOnBehavior, SqlJsonOnBehavior), DbError> {
    let mut on_empty = SqlJsonOnBehavior::Null;
    let mut on_error = SqlJsonOnBehavior::Null;
    // Support two handlers in order. SQL:2016 allows ON EMPTY to appear
    // before ON ERROR; we also tolerate EMPTY/ERROR each exactly once in
    // any order so error messages are clearer on swaps.
    for slot in 0..2 {
        let Some((behavior, kind)) = parse_behavior_on(p)? else {
            break;
        };
        match kind {
            OnKind::Empty => {
                on_empty = behavior;
            }
            OnKind::Error => {
                on_error = behavior;
                // After ON ERROR, no more clauses allowed.
                break;
            }
        }
        if slot == 0 && matches!(kind, OnKind::Empty) {
            continue;
        }
    }
    Ok((on_empty, on_error))
}

enum OnKind {
    Empty,
    Error,
}

fn parse_behavior_on(p: &mut Parser<'_>) -> Result<Option<(SqlJsonOnBehavior, OnKind)>, DbError> {
    // Snapshot current position so we can roll back if what follows is not a
    // behavior clause.
    let saved = p.pos;
    let behavior = if p.eat(&Token::Null) {
        SqlJsonOnBehavior::Null
    } else if p.eat_ident_ci("ERROR") {
        SqlJsonOnBehavior::Error
    } else if p.eat(&Token::Default) {
        SqlJsonOnBehavior::Default(Box::new(parse_expr(p)?))
    } else {
        return Ok(None);
    };

    // Must be followed by `ON EMPTY` or `ON ERROR`; if not, rewind.
    if !p.eat(&Token::On) {
        p.pos = saved;
        return Ok(None);
    }
    if p.eat_ident_ci("EMPTY") {
        Ok(Some((behavior, OnKind::Empty)))
    } else if p.eat_ident_ci("ERROR") {
        Ok(Some((behavior, OnKind::Error)))
    } else {
        Err(DbError::ParseError {
            message: "JSON_TABLE: expected EMPTY or ERROR after ON".into(),
            position: Some(p.current_pos()),
        })
    }
}

/// For EXISTS column: `[ {TRUE | FALSE | UNKNOWN | ERROR} ON ERROR ]`.
fn parse_optional_exists_on_error(p: &mut Parser<'_>) -> Result<SqlJsonOnBehavior, DbError> {
    // Default per PG: `FALSE ON ERROR`.
    let saved = p.pos;
    let behavior = if p.eat(&Token::True) {
        SqlJsonOnBehavior::TrueLit
    } else if p.eat(&Token::False) {
        SqlJsonOnBehavior::FalseLit
    } else if p.eat_ident_ci("UNKNOWN") {
        SqlJsonOnBehavior::Unknown
    } else if p.eat_ident_ci("ERROR") {
        SqlJsonOnBehavior::Error
    } else {
        return Ok(SqlJsonOnBehavior::FalseLit);
    };
    if !p.eat(&Token::On) || !p.eat_ident_ci("ERROR") {
        p.pos = saved;
        return Ok(SqlJsonOnBehavior::FalseLit);
    }
    Ok(behavior)
}
