//! Phase 20.20 — parser for `XMLTABLE(row_xpath PASSING doc COLUMNS (...))`.
//!
//! Dispatched from `parse_from_item` when the identifier `XMLTABLE` is
//! followed by `(`.

use axiomdb_core::error::DbError;

use crate::ast::{FromClause, XmlTable, XmlTableColumn};
use crate::lexer::Token;
use crate::parser::ddl::parse_data_type;
use crate::parser::expr::parse_expr;
use crate::parser::Parser;

/// Parse a `XMLTABLE(...)` FROM-clause item.
///
/// Assumes the `XMLTABLE` identifier has NOT yet been consumed.
/// Returns a fully constructed `FromClause::XmlTable(..)`.
pub(crate) fn parse_xmltable_call(p: &mut Parser<'_>) -> Result<FromClause, DbError> {
    p.expect(&Token::LParen)?;

    // row XPath string literal
    let row_path = p.parse_string_literal()?;

    // PASSING expr [AS name] [, ...]
    let passing = parse_passing(p)?;

    // COLUMNS
    if !p.eat_ident_ci("COLUMNS") {
        return Err(DbError::ParseError {
            message: "XMLTABLE: expected COLUMNS clause".into(),
            position: Some(p.current_pos()),
        });
    }

    let mut columns: Vec<XmlTableColumn> = Vec::new();
    loop {
        columns.push(parse_xmltable_column(p)?);
        if !p.eat(&Token::Comma) {
            break;
        }
    }

    p.expect(&Token::RParen)?;

    // Optional alias
    let alias = if p.eat(&Token::As)
        || matches!(
            p.peek(),
            Token::Ident(_) | Token::QuotedIdent(_) | Token::DqIdent(_)
        ) {
        Some(p.parse_identifier()?)
    } else {
        None
    };

    Ok(FromClause::XmlTable(Box::new(XmlTable {
        row_path,
        passing,
        columns,
        alias,
    })))
}

/// `PASSING expr [AS name] [, ...]`
fn parse_passing(p: &mut Parser<'_>) -> Result<Vec<(crate::expr::Expr, Option<String>)>, DbError> {
    if !p.eat_ident_ci("PASSING") {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    loop {
        let expr = parse_expr(p)?;
        let alias = if p.eat(&Token::As) {
            Some(p.parse_identifier()?)
        } else {
            None
        };
        items.push((expr, alias));
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    Ok(items)
}

/// Parse a single XMLTABLE column definition:
///   `col FOR ORDINALITY`
///   `col type [PATH 'xpath'] [DEFAULT expr] [NOT NULL]`
fn parse_xmltable_column(p: &mut Parser<'_>) -> Result<XmlTableColumn, DbError> {
    let name = p.parse_identifier()?;

    // `name FOR ORDINALITY`
    if p.eat(&Token::For) {
        if !p.eat_ident_ci("ORDINALITY") {
            return Err(DbError::ParseError {
                message: format!("XMLTABLE column `{name}`: expected ORDINALITY after FOR"),
                position: Some(p.current_pos()),
            });
        }
        return Ok(XmlTableColumn {
            name,
            col_type: axiomdb_types::DataType::BigInt,
            path: None,
            default_expr: None,
            not_null: false,
            ordinality: true,
        });
    }

    let parsed = parse_data_type(p)?;
    let col_type = parsed.data_type;

    let path = if p.eat_ident_ci("PATH") {
        Some(p.parse_string_literal()?)
    } else {
        None
    };

    let default_expr = if p.eat(&Token::Default) {
        Some(parse_expr(p)?)
    } else {
        None
    };

    let not_null = if p.eat(&Token::Not) {
        p.expect(&Token::Null)?;
        true
    } else {
        false
    };

    Ok(XmlTableColumn {
        name,
        col_type,
        path,
        default_expr,
        not_null,
        ordinality: false,
    })
}
