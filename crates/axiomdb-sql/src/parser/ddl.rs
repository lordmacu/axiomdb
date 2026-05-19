//! DDL statement parsers — CREATE/DROP DATABASE, CREATE TABLE, CREATE INDEX, DROP TABLE, DROP INDEX.

use axiomdb_catalog::TablePersistence;
use axiomdb_core::error::DbError;
use axiomdb_types::DataType;

use crate::{
    ast::{
        AlterTableOp, AlterTableStmt, ColumnConstraint, ColumnDef, CompositeFieldDef,
        ConstraintDeferrability, ConstraintTiming, CreateAggregateStmt, CreateCompositeTypeStmt,
        CreateEnumTypeStmt, CreateIndexStmt, CreateMaterializedViewStmt, CreateSequenceStmt,
        CreateTableAsSelectStmt, CreateTableLikeStmt, CreateTableStmt, CreateTriggerStmt,
        CreateViewStmt, DropAggregateStmt, DropCompositeTypeStmt, DropIndexStmt,
        DropMaterializedViewStmt, DropSequenceStmt, DropTableStmt, DropTriggerStmt, DropViewStmt,
        ExclusionElement, ExclusionElementTarget, ExclusionOperator, ForeignKeyAction,
        GeneratedColumnKind, IndexColumn, RefreshMaterializedViewStmt, ShowCreateViewStmt,
        SortOrder, Stmt, TableConstraint, TriggerEvent,
    },
    expr::Expr,
    lexer::Token,
    session::normalize_collation_name,
};

use super::{expr::parse_expr, Parser};

// ── CREATE TABLE ──────────────────────────────────────────────────────────────

pub(crate) fn parse_create_database(p: &mut Parser) -> Result<Stmt, DbError> {
    // Optional IF NOT EXISTS
    eat_if_not_exists(p)?;
    let name = p.parse_identifier()?;
    // Consume optional CHARACTER SET / COLLATE / DEFAULT modifiers.
    // mysqldump generates: CREATE DATABASE IF NOT EXISTS `db` DEFAULT CHARACTER SET utf8mb4
    let collation = parse_create_database_options(p)?;
    Ok(Stmt::CreateDatabase(crate::ast::CreateDatabaseStmt {
        name,
        collation,
    }))
}

pub(crate) fn parse_create_trigger(p: &mut Parser) -> Result<Stmt, DbError> {
    let name = p.parse_identifier()?;
    if p.eat(&Token::Before) {
        return Err(DbError::NotImplemented {
            feature: "BEFORE triggers — deferred to Phase 16".into(),
        });
    }
    p.expect(&Token::After)?;
    let event = match p.peek() {
        Token::Insert => {
            p.advance();
            TriggerEvent::Insert
        }
        Token::Update => {
            p.advance();
            TriggerEvent::Update
        }
        Token::Delete => {
            p.advance();
            TriggerEvent::Delete
        }
        other => {
            return Err(DbError::ParseError {
                message: format!("expected INSERT, UPDATE, or DELETE after AFTER, found {other:?}"),
                position: Some(p.current_pos()),
            })
        }
    };
    p.expect(&Token::On)?;
    let table = p.parse_table_ref()?;
    p.expect(&Token::For)?;
    p.expect(&Token::Each)?;
    if p.eat(&Token::Row) {
        return Err(DbError::NotImplemented {
            feature: "FOR EACH ROW triggers — deferred to Phase 16".into(),
        });
    }
    p.expect(&Token::Statement)?;
    p.expect(&Token::As)?;
    let start = p.current_pos();
    p.expect(&Token::Select)?;
    p.skip_until_statement_end();
    let end = p.previous_end();
    let body_sql = p.slice_sql(start, end);
    Ok(Stmt::CreateTrigger(CreateTriggerStmt {
        name,
        event,
        table,
        body_sql,
    }))
}

pub(crate) fn parse_create_aggregate(p: &mut Parser) -> Result<Stmt, DbError> {
    let name = p.parse_identifier()?;
    let arg_types = parse_aggregate_signature(p)?;
    p.expect(&Token::LParen)?;
    let mut sfunc = None;
    let mut stype = None;
    let mut finalfunc = None;
    loop {
        let option_name = p.parse_identifier()?;
        p.expect(&Token::Eq)?;
        if option_name.eq_ignore_ascii_case("sfunc") {
            sfunc = Some(p.parse_identifier()?);
        } else if option_name.eq_ignore_ascii_case("stype") {
            stype = Some(parse_state_type_name(p)?);
        } else if option_name.eq_ignore_ascii_case("finalfunc") {
            finalfunc = Some(p.parse_identifier()?);
        } else {
            return Err(DbError::ParseError {
                message: format!("unsupported CREATE AGGREGATE option '{option_name}'"),
                position: Some(p.current_pos()),
            });
        }
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    p.expect(&Token::RParen)?;
    Ok(Stmt::CreateAggregate(CreateAggregateStmt {
        name,
        arg_types,
        sfunc: sfunc.ok_or_else(|| DbError::ParseError {
            message: "CREATE AGGREGATE requires SFUNC".into(),
            position: Some(p.current_pos()),
        })?,
        stype: stype.ok_or_else(|| DbError::ParseError {
            message: "CREATE AGGREGATE requires STYPE".into(),
            position: Some(p.current_pos()),
        })?,
        finalfunc,
    }))
}

pub(crate) fn parse_create_sequence(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_not_exists = eat_if_not_exists(p)?;
    let sequence = p.parse_table_ref()?;
    let mut start_value = 1i64;
    let mut increment = 1i64;
    let mut min_value = 1i64;
    let mut max_value = i64::MAX;
    let mut cycle = false;
    let mut cache_size = 1u64;

    while !matches!(p.peek(), Token::Eof | Token::Semicolon) {
        match p.peek().clone() {
            Token::Start => {
                p.advance();
                p.eat(&Token::With);
                start_value = parse_sequence_i64(p)?;
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("increment") => {
                p.advance();
                p.eat(&Token::By);
                increment = parse_sequence_i64(p)?;
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("minvalue") => {
                p.advance();
                min_value = parse_sequence_i64(p)?;
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("maxvalue") => {
                p.advance();
                max_value = parse_sequence_i64(p)?;
            }
            Token::No => {
                p.advance();
                if p.eat_ident_ci("minvalue") {
                    min_value = 1;
                } else if p.eat_ident_ci("maxvalue") {
                    max_value = i64::MAX;
                } else if p.eat_ident_ci("cycle") {
                    cycle = false;
                } else {
                    return Err(DbError::ParseError {
                        message: "expected MINVALUE, MAXVALUE, or CYCLE after NO".into(),
                        position: Some(p.current_pos()),
                    });
                }
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("cycle") => {
                p.advance();
                cycle = true;
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("cache") => {
                p.advance();
                let n = parse_sequence_i64(p)?;
                if n < 1 {
                    return Err(DbError::ParseError {
                        message: "CACHE value must be at least 1".into(),
                        position: Some(p.current_pos()),
                    });
                }
                cache_size = n as u64;
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!("unsupported CREATE SEQUENCE option {other:?}"),
                    position: Some(p.current_pos()),
                })
            }
        }
    }

    Ok(Stmt::CreateSequence(CreateSequenceStmt {
        if_not_exists,
        sequence,
        start_value,
        increment,
        min_value,
        max_value,
        cycle,
        cache_size,
    }))
}

/// `CREATE TYPE name AS ENUM (...)` → CreateEnumType
/// `CREATE TYPE name AS (field type[, ...])` → CreateCompositeType
pub(crate) fn parse_create_type(p: &mut Parser) -> Result<Stmt, DbError> {
    let type_ref = p.parse_table_ref()?;
    p.expect(&Token::As)?;
    if p.peek() == &Token::Enum {
        // ENUM path — re-enter the existing function with type_ref already parsed.
        p.advance(); // consume ENUM
        p.expect(&Token::LParen)?;
        if p.eat(&Token::RParen) {
            return Err(DbError::ParseError {
                message: "CREATE TYPE AS ENUM requires at least one label".into(),
                position: Some(p.current_pos()),
            });
        }
        let mut labels = Vec::new();
        loop {
            labels.push(p.parse_string_literal()?);
            if !p.eat(&Token::Comma) {
                break;
            }
        }
        p.expect(&Token::RParen)?;
        return Ok(Stmt::CreateEnumType(CreateEnumTypeStmt {
            enum_type: type_ref,
            labels,
        }));
    }

    // Composite path: AS (field_name type_name [, ...])
    p.expect(&Token::LParen)?;
    let mut fields = Vec::new();
    loop {
        let name = match p.peek().clone() {
            Token::Ident(n) => {
                let s = n.to_string();
                p.advance();
                s
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!("expected field name in composite type, found {other:?}"),
                    position: Some(p.current_pos()),
                });
            }
        };
        let type_name = parse_state_type_name(p)?;
        fields.push(CompositeFieldDef { name, type_name });
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    p.expect(&Token::RParen)?;
    if fields.is_empty() {
        return Err(DbError::ParseError {
            message: "CREATE TYPE AS (...) requires at least one field".into(),
            position: Some(p.current_pos()),
        });
    }
    Ok(Stmt::CreateCompositeType(CreateCompositeTypeStmt {
        type_ref,
        fields,
    }))
}

/// `DROP TYPE [IF EXISTS] name` — dispatches to ENUM or composite at execution time.
///
/// The parser doesn't know at parse time whether the name refers to an enum or
/// a composite, so we produce `DropCompositeType` and let the executor try both.
pub(crate) fn parse_drop_type(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let type_ref = p.parse_table_ref()?;
    Ok(Stmt::DropCompositeType(DropCompositeTypeStmt {
        if_exists,
        type_ref,
    }))
}

fn parse_sequence_i64(p: &mut Parser) -> Result<i64, DbError> {
    let neg = p.eat(&Token::Minus);
    match p.peek().clone() {
        Token::Integer(n) => {
            p.advance();
            Ok(if neg { -n } else { n })
        }
        other => Err(DbError::ParseError {
            message: format!("expected integer sequence option value, found {other:?}"),
            position: Some(p.current_pos()),
        }),
    }
}

/// Consume optional CHARACTER SET / COLLATE / DEFAULT clauses after a database name.
fn parse_create_database_options(p: &mut Parser) -> Result<Option<String>, DbError> {
    let mut collation = None;
    loop {
        match p.peek().clone() {
            // DEFAULT keyword before CHARACTER SET / COLLATE
            Token::Default => {
                p.advance();
            }
            // CHARACTER SET charset_name
            Token::Ident(s) if s.eq_ignore_ascii_case("character") => {
                p.advance(); // CHARACTER
                             // optional SET keyword
                if matches!(p.peek(), Token::Set) {
                    p.advance();
                }
                // charset name (identifier or string)
                let _ = p.parse_identifier();
            }
            // COLLATE collation_name
            Token::Ident(s) if s.eq_ignore_ascii_case("collate") => {
                p.advance(); // COLLATE
                collation = Some(normalize_collation_name(&p.parse_identifier()?)?);
            }
            _ => break Ok(collation),
        }
    }
}

pub(crate) fn parse_create_schema(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_not_exists = eat_if_not_exists(p)?;
    let name = p.parse_identifier()?;
    Ok(Stmt::CreateSchema(crate::ast::CreateSchemaStmt {
        name,
        if_not_exists,
    }))
}

/// Parses everything after `CREATE [TEMP[TORARY]|UNLOGGED] TABLE` has been consumed.
pub(crate) fn parse_create_table(
    p: &mut Parser,
    persistence: TablePersistence,
) -> Result<Stmt, DbError> {
    let if_not_exists = eat_if_not_exists(p)?;
    let new_table = p.parse_table_ref()?;

    // `CREATE TABLE new LIKE src` — copy schema without data
    if p.eat(&Token::Like) {
        let source_table = p.parse_table_ref()?;
        return Ok(Stmt::CreateTableLike(CreateTableLikeStmt {
            if_not_exists,
            new_table,
            source_table,
            persistence,
        }));
    }

    // `CREATE TABLE new AS SELECT ...` or `CREATE TABLE new SELECT ...`
    // MySQL allows both forms (with and without AS).
    p.eat(&Token::As); // consume optional AS
    if matches!(p.peek(), Token::Select) {
        p.advance(); // consume SELECT
        let select = super::dml::parse_select(p)?;
        return Ok(Stmt::CreateTableAsSelect(CreateTableAsSelectStmt {
            new_table,
            select,
            persistence,
        }));
    }

    // Standard form: `CREATE TABLE new (col_defs...)`
    let table = new_table;
    p.expect(&Token::LParen)?;

    let mut columns: Vec<ColumnDef> = Vec::new();
    let mut table_constraints: Vec<TableConstraint> = Vec::new();

    loop {
        if matches!(p.peek(), Token::RParen | Token::Eof) {
            break;
        }
        if is_table_constraint_start(p) {
            table_constraints.push(parse_table_constraint(p)?);
        } else {
            columns.push(parse_column_def(p)?);
        }
        if !p.eat(&Token::Comma) {
            break;
        }
    }

    p.expect(&Token::RParen)?;

    // Consume optional MySQL table options after the closing `)`.
    // mysqldump output always includes: ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 etc.
    let collation = parse_table_options(p)?;

    // Phase 13.9: `IMMUTABLE` table option. Accepted either as a bare
    // keyword after the column list or as `WITH (IMMUTABLE)`.
    let mut immutable = false;
    loop {
        match p.peek().clone() {
            Token::Ident(s) if s.eq_ignore_ascii_case("immutable") => {
                p.advance();
                immutable = true;
            }
            _ => break,
        }
    }

    Ok(Stmt::CreateTable(CreateTableStmt {
        if_not_exists,
        table,
        columns,
        table_constraints,
        collation,
        immutable,
        persistence,
    }))
}

pub(crate) fn parse_create_materialized_view(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_not_exists = eat_if_not_exists(p)?;
    let view = p.parse_table_ref()?;
    p.expect(&Token::As)?;
    let query_start = p.current_pos();
    p.expect(&Token::Select)?;
    let select = super::dml::parse_select(p)?;
    let query_sql = p.slice_sql(query_start, p.previous_end());
    Ok(Stmt::CreateMaterializedView(CreateMaterializedViewStmt {
        if_not_exists,
        view,
        select,
        query_sql,
    }))
}

/// Consume MySQL table-level options that appear after the closing `)` of a
/// CREATE TABLE statement. Examples:
///   `ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci AUTO_INCREMENT=5`
///
/// These are all silently discarded — AxiomDB doesn't use them but must not
/// fail to parse valid MySQL schema dumps.
fn parse_table_options(p: &mut Parser) -> Result<Option<String>, DbError> {
    let mut collation = None;
    loop {
        // Optional DEFAULT before CHARSET / CHARACTER SET / COLLATE
        let had_default = p.eat(&Token::Default);
        match p.peek().clone() {
            // key = value pairs: ENGINE=InnoDB, AUTO_INCREMENT=5, etc.
            // Identifiers that are MySQL-specific option keywords
            Token::Ident(s)
                if s.eq_ignore_ascii_case("engine")
                    || s.eq_ignore_ascii_case("charset")
                    || s.eq_ignore_ascii_case("auto_increment")
                    || s.eq_ignore_ascii_case("comment")
                    || s.eq_ignore_ascii_case("row_format")
                    || s.eq_ignore_ascii_case("key_block_size")
                    || s.eq_ignore_ascii_case("compression")
                    || s.eq_ignore_ascii_case("encryption")
                    || s.eq_ignore_ascii_case("avg_row_length")
                    || s.eq_ignore_ascii_case("max_rows")
                    || s.eq_ignore_ascii_case("min_rows")
                    || s.eq_ignore_ascii_case("pack_keys")
                    || s.eq_ignore_ascii_case("stats_persistent")
                    || s.eq_ignore_ascii_case("checksum")
                    || s.eq_ignore_ascii_case("delay_key_write")
                    || s.eq_ignore_ascii_case("tablespace")
                    || s.eq_ignore_ascii_case("storage")
                    || s.eq_ignore_ascii_case("connection") =>
            {
                p.advance(); // keyword
                if p.eat(&Token::Eq) {
                    skip_table_option_value(p);
                }
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("collate") => {
                p.advance();
                p.eat(&Token::Eq);
                collation = Some(normalize_collation_name(&parse_option_identifier(p)?)?);
            }
            // AUTO_INCREMENT may be lexed as a keyword token rather than Ident
            Token::AutoIncrement => {
                p.advance();
                if p.eat(&Token::Eq) {
                    skip_table_option_value(p);
                }
            }
            // CHARACTER SET charset_name
            Token::Ident(s) if s.eq_ignore_ascii_case("character") => {
                p.advance(); // CHARACTER
                if matches!(p.peek(), Token::Set) {
                    p.advance();
                } // SET
                p.eat(&Token::Eq);
                skip_table_option_value(p);
            }
            // DEFAULT keyword was consumed but nothing followed — stop.
            _ if had_default => break Ok(collation),
            _ => break Ok(collation),
        }
    }
}

/// Consume a single table option value: an identifier, integer, string, or keyword.
fn skip_table_option_value(p: &mut Parser) {
    match p.peek().clone() {
        Token::Ident(_) | Token::QuotedIdent(_) | Token::DqIdent(_) => {
            p.advance();
        }
        Token::Integer(_) | Token::Float(_) | Token::StringLit(_) => {
            p.advance();
        }
        // Keywords that commonly appear as values
        Token::Default | Token::No | Token::Key | Token::Index => {
            p.advance();
        }
        _ => {}
    }
}

fn parse_option_identifier(p: &mut Parser) -> Result<String, DbError> {
    match p.peek().clone() {
        Token::Ident(_) | Token::QuotedIdent(_) | Token::DqIdent(_) => p.parse_identifier(),
        Token::StringLit(s) => {
            p.advance();
            Ok(s)
        }
        _ => Err(DbError::ParseError {
            message: "expected option value".into(),
            position: Some(p.current_pos()),
        }),
    }
}

fn is_table_constraint_start(p: &Parser) -> bool {
    matches!(
        p.peek(),
        Token::Primary
            | Token::Unique
            | Token::Foreign
            | Token::Check
            | Token::Exclude
            | Token::Constraint
            | Token::Index  // inline INDEX idx (col) — MySQL extension
            | Token::Key // inline KEY idx (col) — MySQL extension
    )
}

// ── Column definition ─────────────────────────────────────────────────────────

fn parse_column_def(p: &mut Parser) -> Result<ColumnDef, DbError> {
    let name = p.parse_identifier()?;

    // Serial shorthands: BIGSERIAL → BigInt, SERIAL → Int, SMALLSERIAL → SmallInt.
    let serial_type: Option<DataType> = if matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("BIGSERIAL"))
    {
        p.advance();
        Some(DataType::BigInt)
    } else if matches!(p.peek(), Token::Serial) {
        p.advance();
        Some(DataType::Int)
    } else if matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("SMALLSERIAL")) {
        p.advance();
        Some(DataType::SmallInt)
    } else {
        None
    };

    let (data_type, type_len, is_char, declared_type_name, array_ndims, array_size_hints) =
        if let Some(dt) = serial_type.clone() {
            (dt, 0u16, false, None::<crate::ast::TableRef>, 0u8, vec![])
        } else {
            parse_column_data_type(p)?
        };
    let mut constraints = Vec::new();
    if serial_type.is_some() {
        constraints.push(ColumnConstraint::AutoIncrement);
    }
    let mut collation = None;

    loop {
        match p.peek() {
            Token::Not => {
                p.advance();
                p.expect(&Token::Null)?;
                constraints.push(ColumnConstraint::NotNull);
            }
            Token::Null => {
                p.advance();
                constraints.push(ColumnConstraint::Null);
            }
            Token::Default => {
                p.advance();
                let expr = parse_expr(p)?;
                constraints.push(ColumnConstraint::Default(expr));
            }
            Token::Primary => {
                p.advance();
                p.expect(&Token::Key)?;
                constraints.push(ColumnConstraint::PrimaryKey);
            }
            Token::Unique => {
                p.advance();
                // Optional KEY or INDEX keyword (MySQL syntax)
                p.eat(&Token::Key);
                p.eat(&Token::Index);
                constraints.push(ColumnConstraint::Unique);
            }
            Token::AutoIncrement => {
                p.advance();
                constraints.push(ColumnConstraint::AutoIncrement);
            }
            Token::Serial => {
                // 4.3c: SERIAL is synonym for AUTO_INCREMENT
                p.advance();
                constraints.push(ColumnConstraint::AutoIncrement);
            }
            Token::References => {
                constraints.push(parse_column_references(p)?);
            }
            Token::Check => {
                // 4.3b
                constraints.push(parse_check_column_constraint(p)?);
            }
            Token::Ident(s) if s.eq_ignore_ascii_case("generated") => {
                constraints.push(parse_generated_column_constraint(p)?);
            }
            // ── MySQL column attributes (4.1c) ────────────────────────────────
            // UNSIGNED / ZEROFILL / SIGNED — modifiers on numeric types.
            // Parsed and discarded; AxiomDB stores all integers as signed.
            Token::Ident(s)
                if s.eq_ignore_ascii_case("unsigned")
                    || s.eq_ignore_ascii_case("zerofill")
                    || s.eq_ignore_ascii_case("signed") =>
            {
                p.advance();
            }
            // COLLATE collation_name
            Token::Ident(s) if s.eq_ignore_ascii_case("collate") => {
                p.advance();
                collation = Some(normalize_collation_name(&p.parse_identifier()?)?);
            }
            // CHARACTER SET charset_name
            Token::Ident(s) if s.eq_ignore_ascii_case("character") => {
                p.advance(); // CHARACTER
                if matches!(p.peek(), Token::Set) {
                    p.advance();
                } // SET
                let _ = p.parse_identifier()?;
            }
            // COMMENT 'text'
            Token::Ident(s) if s.eq_ignore_ascii_case("comment") => {
                p.advance();
                if matches!(p.peek(), Token::StringLit(_)) {
                    p.advance();
                }
            }
            // VISIBLE / INVISIBLE — MySQL 8.0 index visibility
            Token::Ident(s)
                if s.eq_ignore_ascii_case("visible") || s.eq_ignore_ascii_case("invisible") =>
            {
                p.advance();
            }
            // ON UPDATE expr — auto-update trigger (e.g. ON UPDATE CURRENT_TIMESTAMP)
            Token::On => {
                p.advance(); // ON
                if matches!(p.peek(), Token::Update) {
                    p.advance(); // UPDATE
                    let expr = parse_expr(p)?;
                    constraints.push(crate::ast::ColumnConstraint::OnUpdate(expr));
                }
                // If not UPDATE, we consumed ON spuriously — but ON only appears
                // here in "ON UPDATE" context for column defs, so this is safe.
            }
            // STORAGE DEFAULT|DISK|MEMORY — NDB engine storage attribute
            Token::Ident(s) if s.eq_ignore_ascii_case("storage") => {
                p.advance();
                let _ = p.parse_identifier();
            }
            _ => break,
        }
    }

    Ok(ColumnDef {
        name,
        data_type,
        declared_type_name,
        constraints,
        collation,
        type_len,
        is_char,
        array_ndims: if array_ndims > 0 {
            Some(array_ndims)
        } else {
            None
        },
        array_size_hints,
    })
}

#[allow(clippy::type_complexity)]
fn parse_column_data_type(
    p: &mut Parser,
) -> Result<
    (
        DataType,
        u16,
        bool,
        Option<crate::ast::TableRef>,
        u8,
        Vec<Option<u16>>,
    ),
    DbError,
> {
    match parse_data_type(p) {
        Ok(parsed) => Ok((
            parsed.data_type,
            parsed.type_len,
            parsed.is_char,
            None,
            parsed.ndims,
            parsed.size_hints,
        )),
        Err(err) => {
            if is_custom_type_start(p.peek()) {
                let type_name = p.parse_table_ref()?;
                Ok((DataType::Text, 0, false, Some(type_name), 0, vec![]))
            } else {
                Err(err)
            }
        }
    }
}

fn is_custom_type_start(tok: &Token<'_>) -> bool {
    matches!(
        tok,
        Token::Ident(_) | Token::QuotedIdent(_) | Token::DqIdent(_)
    )
}

fn parse_generated_column_constraint(p: &mut Parser) -> Result<ColumnConstraint, DbError> {
    p.advance(); // GENERATED

    // GENERATED BY DEFAULT AS IDENTITY
    if p.eat(&Token::By) {
        // DEFAULT is a real keyword (Token::Default), not an identifier.
        if !p.eat(&Token::Default) {
            return Err(DbError::ParseError {
                message: "expected DEFAULT after BY in GENERATED BY DEFAULT".into(),
                position: Some(p.current_pos()),
            });
        }
        p.expect(&Token::As)?;
        if !p.eat_ident_ci("identity") {
            return Err(DbError::ParseError {
                message: "expected IDENTITY after GENERATED BY DEFAULT AS".into(),
                position: Some(p.current_pos()),
            });
        }
        parse_identity_options(p)?;
        return Ok(ColumnConstraint::GeneratedIdentity { by_default: true });
    }

    // GENERATED ALWAYS AS ...
    if !p.eat_ident_ci("always") {
        return Err(DbError::ParseError {
            message: "expected ALWAYS or BY DEFAULT after GENERATED".into(),
            position: Some(p.current_pos()),
        });
    }
    p.expect(&Token::As)?;

    // GENERATED ALWAYS AS IDENTITY
    if p.eat_ident_ci("identity") {
        parse_identity_options(p)?;
        return Ok(ColumnConstraint::GeneratedIdentity { by_default: false });
    }

    // GENERATED ALWAYS AS (expr) STORED|VIRTUAL
    p.expect(&Token::LParen)?;
    let expr = parse_expr(p)?;
    p.expect(&Token::RParen)?;
    let kind = if p.eat_ident_ci("stored") {
        GeneratedColumnKind::Stored
    } else {
        let _ = p.eat_ident_ci("virtual");
        GeneratedColumnKind::Virtual
    };
    Ok(ColumnConstraint::Generated { expr, kind })
}

fn parse_identity_options(p: &mut Parser) -> Result<(), DbError> {
    if p.eat(&Token::LParen) {
        let mut depth = 1usize;
        loop {
            match p.peek() {
                Token::LParen => {
                    depth += 1;
                    p.advance();
                }
                Token::RParen => {
                    depth -= 1;
                    p.advance();
                    if depth == 0 {
                        break;
                    }
                }
                Token::Eof => {
                    return Err(DbError::ParseError {
                        message: "unexpected EOF in identity options".into(),
                        position: None,
                    });
                }
                _ => {
                    p.advance();
                }
            }
        }
    }
    Ok(())
}

// ── Table-level constraint ────────────────────────────────────────────────────

fn parse_table_constraint(p: &mut Parser) -> Result<TableConstraint, DbError> {
    // Optional CONSTRAINT name prefix
    let name: Option<String> = if p.eat(&Token::Constraint) {
        Some(p.parse_identifier()?)
    } else {
        None
    };

    match p.peek() {
        Token::Primary => {
            p.advance();
            p.expect(&Token::Key)?;
            let columns = parse_ident_list_paren(p)?;
            Ok(TableConstraint::PrimaryKey { name, columns })
        }
        Token::Unique => {
            p.advance();
            // Optional INDEX / KEY keyword (MySQL)
            p.eat(&Token::Index);
            p.eat(&Token::Key);
            let columns = parse_ident_list_paren(p)?;
            Ok(TableConstraint::Unique { name, columns })
        }
        Token::Foreign => {
            p.advance();
            p.expect(&Token::Key)?;
            let columns = parse_ident_list_paren(p)?;
            p.expect(&Token::References)?;
            let ref_table = p.parse_identifier()?;
            let ref_columns = parse_ident_list_paren(p)?;
            let (on_delete, on_update) = parse_fk_actions(p)?;
            Ok(TableConstraint::ForeignKey {
                name,
                columns,
                ref_table,
                ref_columns,
                on_delete,
                on_update,
                deferrability: parse_fk_deferrability(p)?,
            })
        }
        Token::Check => {
            p.advance();
            p.expect(&Token::LParen)?;
            let expr = parse_expr(p)?;
            p.expect(&Token::RParen)?;
            Ok(TableConstraint::Check { name, expr })
        }
        Token::Exclude => parse_exclusion_constraint(p, name),
        // ── MySQL inline INDEX / KEY ──────────────────────────────────────────
        // `INDEX idx_name (col1, col2)` and `KEY idx_name (col1, col2)` inside
        // the column list are MySQL extensions for non-unique indexes.
        // We parse them as TableConstraint::Index (non-unique).
        Token::Index | Token::Key => {
            p.advance(); // consume INDEX or KEY
            // Optional index name (may be absent in some MySQL dumps)
            let idx_name = match p.peek() {
                Token::LParen => None,
                _ => Some(p.parse_identifier()?),
            };
            // Column list: (col1 [ASC|DESC], col2 ...)
            let columns = parse_index_column_list_paren(p)?;
            // Optionally consume index options: USING BTREE / HASH, COMMENT, etc.
            skip_index_options(p);
            Ok(TableConstraint::Index {
                name: idx_name,
                columns,
            })
        }
        other => Err(DbError::ParseError {
            message: format!(
                "expected PRIMARY, UNIQUE, FOREIGN, CHECK, EXCLUDE, INDEX, or KEY in table constraint, found {:?}",
                other,
            ),
            position: Some(p.current_pos()),
        }),
    }
}

fn parse_exclusion_constraint(
    p: &mut Parser,
    name: Option<String>,
) -> Result<TableConstraint, DbError> {
    p.expect(&Token::Exclude)?;
    p.expect(&Token::Using)?;
    let using = p.parse_identifier()?;
    p.expect(&Token::LParen)?;
    let mut elements = Vec::new();
    loop {
        elements.push(parse_exclusion_element(p)?);
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    p.expect(&Token::RParen)?;
    let predicate = if p.eat(&Token::Where) {
        p.expect(&Token::LParen)?;
        let expr = parse_expr(p)?;
        p.expect(&Token::RParen)?;
        Some(expr)
    } else {
        None
    };
    Ok(TableConstraint::Exclude {
        name,
        using,
        elements,
        predicate,
    })
}

fn parse_exclusion_element(p: &mut Parser) -> Result<ExclusionElement, DbError> {
    let target = if p.eat(&Token::LParen) {
        let expr = parse_expr(p)?;
        p.expect(&Token::RParen)?;
        ExclusionElementTarget::Expr(expr)
    } else {
        ExclusionElementTarget::Column(p.parse_identifier()?)
    };
    p.expect(&Token::With)?;
    let operator = parse_exclusion_operator(p)?;
    Ok(ExclusionElement { target, operator })
}

fn parse_exclusion_operator(p: &mut Parser) -> Result<ExclusionOperator, DbError> {
    let op = match p.peek() {
        Token::Eq => {
            p.advance();
            ExclusionOperator::Eq
        }
        Token::NotEq => {
            p.advance();
            ExclusionOperator::NotEq
        }
        Token::Lt => {
            p.advance();
            ExclusionOperator::Lt
        }
        Token::LtEq => {
            p.advance();
            ExclusionOperator::LtEq
        }
        Token::Gt => {
            p.advance();
            ExclusionOperator::Gt
        }
        Token::GtEq => {
            p.advance();
            ExclusionOperator::GtEq
        }
        Token::Amp if matches!(p.peek_at(1), Token::Amp) => {
            p.advance();
            p.advance();
            ExclusionOperator::Overlaps
        }
        other => {
            return Err(DbError::ParseError {
                message: format!("expected exclusion operator after WITH, found {:?}", other),
                position: Some(p.current_pos()),
            })
        }
    };
    Ok(op)
}

// ── REFERENCES (column-level) ─────────────────────────────────────────────────

fn parse_column_references(p: &mut Parser) -> Result<ColumnConstraint, DbError> {
    p.advance(); // consume REFERENCES
    let table = p.parse_identifier()?;

    let column = if p.eat(&Token::LParen) {
        let col = p.parse_identifier()?;
        p.expect(&Token::RParen)?;
        Some(col)
    } else {
        None
    };

    let (on_delete, on_update) = parse_fk_actions(p)?;

    Ok(ColumnConstraint::References {
        table,
        column,
        on_delete,
        on_update,
        deferrability: parse_fk_deferrability(p)?,
    })
}

// ── FK actions ────────────────────────────────────────────────────────────────

fn parse_fk_actions(p: &mut Parser) -> Result<(ForeignKeyAction, ForeignKeyAction), DbError> {
    let mut on_delete = ForeignKeyAction::NoAction;
    let mut on_update = ForeignKeyAction::NoAction;

    loop {
        if !matches!(p.peek(), Token::On) {
            break;
        }
        p.advance(); // consume ON
        match p.peek() {
            Token::Delete => {
                p.advance();
                on_delete = parse_fk_action(p)?;
            }
            Token::Update => {
                p.advance();
                on_update = parse_fk_action(p)?;
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!("expected DELETE or UPDATE after ON, found {:?}", other,),
                    position: Some(p.current_pos()),
                });
            }
        }
    }

    Ok((on_delete, on_update))
}

fn parse_fk_action(p: &mut Parser) -> Result<ForeignKeyAction, DbError> {
    match p.peek() {
        Token::Cascade => {
            p.advance();
            Ok(ForeignKeyAction::Cascade)
        }
        Token::Restrict => {
            p.advance();
            Ok(ForeignKeyAction::Restrict)
        }
        Token::Set => {
            p.advance();
            match p.peek() {
                Token::Null => {
                    p.advance();
                    Ok(ForeignKeyAction::SetNull)
                }
                Token::Default => {
                    p.advance();
                    Ok(ForeignKeyAction::SetDefault)
                }
                other => Err(DbError::ParseError {
                    message: format!(
                        "expected NULL or DEFAULT after SET in FK action, found {:?}",
                        other,
                    ),
                    position: Some(p.current_pos()),
                }),
            }
        }
        Token::No => {
            p.advance();
            p.expect(&Token::Action)?;
            Ok(ForeignKeyAction::NoAction)
        }
        other => Err(DbError::ParseError {
            message: format!(
                "expected CASCADE, RESTRICT, SET NULL, SET DEFAULT, or NO ACTION in FK action, found {:?}",
                other,
            ),
            position: Some(p.current_pos()),
        }),
    }
}

fn parse_fk_deferrability(p: &mut Parser) -> Result<ConstraintDeferrability, DbError> {
    let mut deferrability = ConstraintDeferrability::default();

    match p.peek() {
        Token::Deferrable => {
            p.advance();
            deferrability.deferrable = true;
        }
        Token::Not if matches!(p.peek_at(1), Token::Deferrable) => {
            p.advance();
            p.advance();
        }
        _ => return Ok(deferrability),
    }

    if p.eat(&Token::Initially) {
        deferrability.initially = match p.peek() {
            Token::Deferred => {
                p.advance();
                ConstraintTiming::Deferred
            }
            Token::Immediate => {
                p.advance();
                ConstraintTiming::Immediate
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!(
                        "expected DEFERRED or IMMEDIATE after INITIALLY, found {:?}",
                        other
                    ),
                    position: Some(p.current_pos()),
                });
            }
        };
    }

    if !deferrability.deferrable && deferrability.initially == ConstraintTiming::Deferred {
        return Err(DbError::ParseError {
            message: "NOT DEFERRABLE cannot be INITIALLY DEFERRED".into(),
            position: Some(p.current_pos()),
        });
    }

    Ok(deferrability)
}

// ── Inline index helpers ──────────────────────────────────────────────────────

/// Parse `(col1 [ASC|DESC], col2 ...)` for an inline INDEX/KEY definition.
/// Returns just the column names (direction discarded — non-unique indexes
/// are stored without direction in AxiomDB's catalog).
fn parse_index_column_list_paren(p: &mut Parser) -> Result<Vec<String>, DbError> {
    p.expect(&Token::LParen)?;
    let mut columns = Vec::new();
    loop {
        let col = p.parse_identifier()?;
        columns.push(col);
        // Optional column length: col(255)
        if p.eat(&Token::LParen) {
            // consume length spec
            while !matches!(p.peek(), Token::RParen | Token::Eof) {
                p.advance();
            }
            p.eat(&Token::RParen);
        }
        // Optional ASC / DESC
        p.eat(&Token::Asc);
        p.eat(&Token::Desc);
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    p.expect(&Token::RParen)?;
    Ok(columns)
}

/// Consume optional index options that may follow the column list:
/// `USING BTREE|HASH`, `COMMENT 'text'`, `KEY_BLOCK_SIZE=N`, etc.
fn skip_index_options(p: &mut Parser) {
    loop {
        match p.peek().clone() {
            Token::Using => {
                p.advance(); // USING
                let _ = p.parse_identifier(); // BTREE / HASH
            }
            Token::Ident(s)
                if s.eq_ignore_ascii_case("comment")
                    || s.eq_ignore_ascii_case("key_block_size")
                    || s.eq_ignore_ascii_case("with")
                    || s.eq_ignore_ascii_case("parser")
                    || s.eq_ignore_ascii_case("invisible")
                    || s.eq_ignore_ascii_case("visible") =>
            {
                p.advance();
                if p.eat(&Token::Eq) {
                    skip_table_option_value(p);
                } else {
                    match p.peek() {
                        Token::StringLit(_) | Token::Integer(_) | Token::Ident(_) => {
                            p.advance();
                        }
                        _ => {}
                    }
                }
            }
            _ => break,
        }
    }
}

// ── CHECK (column-level) ──────────────────────────────────────────────────────

fn parse_check_column_constraint(p: &mut Parser) -> Result<ColumnConstraint, DbError> {
    p.advance(); // consume CHECK
    p.expect(&Token::LParen)?;
    let expr = parse_expr(p)?;
    p.expect(&Token::RParen)?;
    Ok(ColumnConstraint::Check(expr))
}

// ── Data type ─────────────────────────────────────────────────────────────────

/// Result of parsing a data type, including array metadata.
///
/// `ndims` is 0 for non-arrays, 1-6 for arrays.
/// `size_hints` has one entry per dimension (length = ndims), with `None` for unbounded.
pub(crate) struct ParsedDataType {
    pub data_type: DataType,
    pub type_len: u16,
    pub is_char: bool,
    pub ndims: u8,
    pub size_hints: Vec<Option<u16>>,
}

/// Parses a SQL data type keyword, returning `(DataType, type_len, is_char, ndims, size_hints)`.
///
/// `type_len` is the `N` from `VARCHAR(N)` / `CHAR(N)`, or `0` for all other types.
/// `is_char` is `true` only for `CHAR(N)` declarations (fixed-length).
/// `ndims` is the number of array dimensions (0 for non-arrays, 1-6 for arrays).
/// `size_hints` has one entry per dimension with `None` for unbounded.
pub(crate) fn parse_data_type(p: &mut Parser) -> Result<ParsedDataType, DbError> {
    let pos = p.current_pos();
    let (data_type, type_len, is_char) = match p.peek().clone() {
        Token::TyInt | Token::TyInteger => {
            p.advance();
            (DataType::Int, 0, false)
        }
        Token::TyBigint => {
            p.advance();
            (DataType::BigInt, 0, false)
        }
        Token::TyReal | Token::TyFloat => {
            p.advance();
            eat_optional_length(p)?; // FLOAT(n) — precision hint, ignored
            (DataType::Float, 0, false)
        }
        Token::TyDouble => {
            p.advance();
            // eat optional PRECISION keyword: DOUBLE PRECISION
            if matches!(p.peek(), Token::Ident(s) if s.eq_ignore_ascii_case("PRECISION")) {
                p.advance();
            }
            (DataType::Real, 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("FLOAT4") => {
            p.advance();
            (DataType::Float, 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("FLOAT8") => {
            p.advance();
            (DataType::Real, 0, false)
        }
        Token::TyDecimal | Token::TyNumeric => {
            p.advance();
            let (prec, scale) = parse_decimal_params(p)?;
            let type_len = ((prec as u16) << 8) | (scale as u16);
            (DataType::Decimal, type_len, false)
        }
        Token::TyBool | Token::TyBoolean => {
            p.advance();
            (DataType::Bool, 0, false)
        }
        Token::TyText => {
            p.advance();
            (DataType::Text, 0, false)
        }
        Token::TyVarchar => {
            p.advance();
            let len = eat_optional_length(p)?;
            (DataType::Text, len, false)
        }
        Token::TyChar => {
            p.advance();
            let len = eat_optional_length(p)?;
            (DataType::Text, len, true)
        }
        Token::TyBlob | Token::TyBytea => {
            p.advance();
            (DataType::Bytes, 0, false)
        }
        Token::TyDate => {
            p.advance();
            (DataType::Date, 0, false)
        }
        Token::TyTimestamp | Token::TyDatetime => {
            p.advance();
            (DataType::Timestamp, 0, false)
        }
        Token::TyUuid => {
            p.advance();
            (DataType::Uuid, 0, false)
        }
        Token::TyJson => {
            p.advance();
            (DataType::Json, 0, false)
        }
        Token::TyJsonb => {
            p.advance();
            (DataType::Jsonb, 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("TINYINT") => {
            p.advance();
            eat_optional_length(p)?; // TINYINT(N) — display width, ignored
            (DataType::TinyInt, 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("SMALLINT") => {
            p.advance();
            eat_optional_length(p)?;
            (DataType::SmallInt, 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("MEDIUMINT") => {
            p.advance();
            eat_optional_length(p)?;
            (DataType::Int, 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("YEAR") => {
            p.advance();
            eat_optional_length(p)?; // YEAR(4)
            (DataType::Int, 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("TIME") => {
            p.advance();
            (DataType::Timestamp, 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("INT4RANGE") => {
            p.advance();
            (DataType::Range(Box::new(DataType::Int)), 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("INT8RANGE") => {
            p.advance();
            (DataType::Range(Box::new(DataType::BigInt)), 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("NUMRANGE") => {
            p.advance();
            (DataType::Range(Box::new(DataType::Decimal)), 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("DATERANGE") => {
            p.advance();
            (DataType::Range(Box::new(DataType::Date)), 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("TSRANGE") => {
            p.advance();
            (DataType::Range(Box::new(DataType::Timestamp)), 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("MONEY") => {
            p.advance();
            (DataType::Money, 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("LTREE") => {
            p.advance();
            (DataType::Ltree, 0, false)
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("XML") || s.eq_ignore_ascii_case("XMLTYPE") => {
            p.advance();
            (DataType::Xml, 0, false)
        }
        other => {
            return Err(DbError::ParseError {
                message: format!(
                    "expected a data type (INT, TEXT, BIGINT, …) but found {:?}",
                    other,
                ),
                position: Some(pos),
            });
        }
    };

    // Parse array suffixes: [] brackets and/or ARRAY keyword
    let mut ndims = 0u8;
    let mut size_hints: Vec<Option<u16>> = Vec::new();

    loop {
        if p.eat(&Token::LBracket) {
            // Parse optional size in brackets, e.g. [3] in FLOAT[3][3]
            let size_hint = if matches!(p.peek(), Token::Integer(_)) {
                match p.peek() {
                    Token::Integer(n) => {
                        let size = (*n).min(u16::MAX as i64).max(1) as u16;
                        p.advance();
                        Some(size)
                    }
                    _ => None,
                }
            } else {
                None
            };
            p.expect(&Token::RBracket)?;
            size_hints.push(size_hint);
            ndims += 1;
        } else if p.eat(&Token::Array) {
            // `BOOL ARRAY` suffix — PG-compatible
            ndims = ndims.max(1);
        } else {
            break;
        }
    }

    // Validate ndims (max 6 per PG spec)
    if ndims > 6 {
        return Err(DbError::InvalidValue {
            reason: "number of array dimensions exceeds the maximum allowed (6)".into(),
        });
    }

    Ok(ParsedDataType {
        data_type,
        type_len,
        is_char,
        ndims,
        size_hints,
    })
}

fn parse_aggregate_signature(p: &mut Parser) -> Result<Vec<DataType>, DbError> {
    p.expect(&Token::LParen)?;
    let mut arg_types = Vec::new();
    if p.eat(&Token::RParen) {
        return Ok(arg_types);
    }
    loop {
        let parsed = parse_data_type(p)?;
        arg_types.push(parsed.data_type);
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    p.expect(&Token::RParen)?;
    Ok(arg_types)
}

fn parse_state_type_name(p: &mut Parser) -> Result<String, DbError> {
    let mut name = match p.peek().clone() {
        Token::TyInt => {
            p.advance();
            "INT".to_string()
        }
        Token::TyInteger => {
            p.advance();
            "INTEGER".to_string()
        }
        Token::TyBigint => {
            p.advance();
            "BIGINT".to_string()
        }
        Token::TyReal => {
            p.advance();
            "REAL".to_string()
        }
        Token::TyDouble => {
            p.advance();
            "DOUBLE".to_string()
        }
        Token::TyFloat => {
            p.advance();
            "FLOAT".to_string()
        }
        Token::TyDecimal => {
            p.advance();
            "DECIMAL".to_string()
        }
        Token::TyNumeric => {
            p.advance();
            "NUMERIC".to_string()
        }
        Token::TyBool => {
            p.advance();
            "BOOL".to_string()
        }
        Token::TyBoolean => {
            p.advance();
            "BOOLEAN".to_string()
        }
        Token::TyText => {
            p.advance();
            "TEXT".to_string()
        }
        Token::TyVarchar => {
            p.advance();
            "VARCHAR".to_string()
        }
        Token::TyChar => {
            p.advance();
            "CHAR".to_string()
        }
        Token::TyBlob => {
            p.advance();
            "BLOB".to_string()
        }
        Token::TyBytea => {
            p.advance();
            "BYTEA".to_string()
        }
        Token::TyDate => {
            p.advance();
            "DATE".to_string()
        }
        Token::TyTimestamp => {
            p.advance();
            "TIMESTAMP".to_string()
        }
        Token::TyDatetime => {
            p.advance();
            "DATETIME".to_string()
        }
        Token::TyUuid => {
            p.advance();
            "UUID".to_string()
        }
        Token::TyJson => {
            p.advance();
            "JSON".to_string()
        }
        Token::TyJsonb => {
            p.advance();
            "JSONB".to_string()
        }
        _ => p.parse_identifier()?,
    };
    while p.eat(&Token::LBracket) {
        p.expect(&Token::RBracket)?;
        name.push_str("[]");
    }
    Ok(name)
}

/// Parse optional `(precision [, scale])` for DECIMAL/NUMERIC.
/// Returns `(precision, scale)` — defaults to `(10, 0)` when omitted.
/// Validates: 1 ≤ precision ≤ 38, 0 ≤ scale ≤ precision.
fn parse_decimal_params(p: &mut Parser) -> Result<(u8, u8), DbError> {
    if !p.eat(&Token::LParen) {
        return Ok((10, 0));
    }
    let prec = match p.peek() {
        Token::Integer(n) => {
            let v = *n;
            p.advance();
            v
        }
        _ => {
            return Err(DbError::ParseError {
                message: "expected precision integer in DECIMAL type parameters".into(),
                position: Some(p.current_pos()),
            });
        }
    };
    if !(1..=38).contains(&prec) {
        return Err(DbError::ParseError {
            message: format!("DECIMAL precision must be between 1 and 38, got {prec}"),
            position: Some(p.current_pos()),
        });
    }
    let scale = if p.eat(&Token::Comma) {
        match p.peek() {
            Token::Integer(n) => {
                let v = *n;
                p.advance();
                v
            }
            _ => {
                return Err(DbError::ParseError {
                    message: "expected scale integer after comma in DECIMAL type parameters".into(),
                    position: Some(p.current_pos()),
                });
            }
        }
    } else {
        0
    };
    if scale > prec {
        return Err(DbError::ParseError {
            message: format!("DECIMAL scale ({scale}) cannot exceed precision ({prec})"),
            position: Some(p.current_pos()),
        });
    }
    p.expect(&Token::RParen)?;
    Ok((prec as u8, scale as u8))
}

fn eat_optional_length(p: &mut Parser) -> Result<u16, DbError> {
    if p.eat(&Token::LParen) {
        let len = match p.peek() {
            Token::Integer(n) => {
                let v = (*n).min(u16::MAX as i64).max(0) as u16;
                p.advance();
                v
            }
            _ => {
                return Err(DbError::ParseError {
                    message: "expected length integer in type parameter".into(),
                    position: Some(p.current_pos()),
                });
            }
        };
        p.expect(&Token::RParen)?;
        Ok(len)
    } else {
        Ok(0)
    }
}

// ── Identifier list ───────────────────────────────────────────────────────────

fn parse_ident_list_paren(p: &mut Parser) -> Result<Vec<String>, DbError> {
    p.expect(&Token::LParen)?;
    let mut names = vec![p.parse_identifier()?];
    while p.eat(&Token::Comma) {
        names.push(p.parse_identifier()?);
    }
    p.expect(&Token::RParen)?;
    Ok(names)
}

// ── IF NOT EXISTS / IF EXISTS ─────────────────────────────────────────────────

fn eat_if_not_exists(p: &mut Parser) -> Result<bool, DbError> {
    if p.eat(&Token::If) {
        p.expect(&Token::Not)?;
        p.expect(&Token::Exists)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

fn eat_if_exists(p: &mut Parser) -> Result<bool, DbError> {
    if p.eat(&Token::If) {
        p.expect(&Token::Exists)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

// ── CREATE INDEX ──────────────────────────────────────────────────────────────

/// Parses everything after `CREATE [UNIQUE] INDEX` has been consumed.
pub(crate) fn parse_create_index(p: &mut Parser, unique: bool) -> Result<Stmt, DbError> {
    let if_not_exists = eat_if_not_exists(p)?;
    let name = p.parse_identifier()?;
    p.expect(&Token::On)?;
    let table = p.parse_table_ref()?;

    // Optional USING method (Phase 11.1b): btree (default) or brin.
    let index_type = if p.eat(&Token::Using) {
        let method = p.parse_identifier()?.to_lowercase();
        match method.as_str() {
            "btree" => crate::ast::IndexType::BTree,
            "brin" => crate::ast::IndexType::Brin,
            "trigram" | "gin_trgm" => crate::ast::IndexType::Trigram,
            "fts" | "fulltext" => crate::ast::IndexType::FullText,
            "gin" => crate::ast::IndexType::Gin,
            other => {
                return Err(DbError::ParseError {
                    message: format!(
                        "unknown index method: {other}; supported: btree, brin, trigram, fts, gin"
                    ),
                    position: Some(p.current_pos()),
                });
            }
        }
    } else {
        crate::ast::IndexType::BTree
    };

    p.expect(&Token::LParen)?;
    let mut columns = vec![parse_index_column(p)?];
    while p.eat(&Token::Comma) {
        columns.push(parse_index_column(p)?);
    }
    p.expect(&Token::RParen)?;

    // Optional INCLUDE (col1, col2, ...) for covering indexes (Phase 6.13).
    let include_columns: Vec<String> = if p.eat(&Token::Include) {
        p.expect(&Token::LParen)?;
        let mut cols = vec![p.parse_identifier()?];
        while p.eat(&Token::Comma) {
            cols.push(p.parse_identifier()?);
        }
        p.expect(&Token::RParen)?;
        cols
    } else {
        vec![]
    };

    // Optional WHERE predicate for partial indexes (Phase 6.7).
    let predicate = if p.eat(&Token::Where) {
        Some(parse_expr(p)?)
    } else {
        None
    };

    // Optional WITH (key = value, ...) storage options (Phase 6.8 + 11.1b).
    // Supported: `fillfactor` (B-Tree), `pages_per_range` (BRIN).
    let mut fillfactor: Option<u8> = None;
    let mut pages_per_range: Option<u32> = None;
    if p.eat(&Token::With) {
        p.expect(&Token::LParen)?;
        loop {
            let key = p.parse_identifier()?.to_lowercase();
            p.expect(&Token::Eq)?;
            let val = match p.peek() {
                Token::Integer(n) => {
                    let n = *n;
                    p.advance();
                    n
                }
                other => {
                    return Err(DbError::ParseError {
                        message: format!("{key} must be an integer, found {other:?}"),
                        position: Some(p.current_pos()),
                    });
                }
            };
            match key.as_str() {
                "fillfactor" => {
                    if !(10..=100).contains(&val) {
                        return Err(DbError::ParseError {
                            message: "fillfactor must be between 10 and 100".into(),
                            position: Some(p.current_pos()),
                        });
                    }
                    fillfactor = Some(val as u8);
                }
                "pages_per_range" => {
                    if !(1..=65536).contains(&val) {
                        return Err(DbError::ParseError {
                            message: "pages_per_range must be between 1 and 65536".into(),
                            position: Some(p.current_pos()),
                        });
                    }
                    pages_per_range = Some(val as u32);
                }
                other => {
                    return Err(DbError::ParseError {
                        message: format!("unknown index option: {other}"),
                        position: Some(p.current_pos()),
                    });
                }
            }
            if !p.eat(&Token::Comma) {
                break;
            }
        }
        p.expect(&Token::RParen)?;
    }

    Ok(Stmt::CreateIndex(CreateIndexStmt {
        if_not_exists,
        unique,
        name,
        table,
        columns,
        include_columns,
        predicate,
        fillfactor,
        index_type,
        pages_per_range,
    }))
}

fn parse_index_column(p: &mut Parser) -> Result<IndexColumn, DbError> {
    // Phase 21.8: An index column is either
    //   • a simple column reference (the common case):       (col_name [ASC|DESC])
    //   • an expression over one or more columns (Phase 21.8): (expr [ASC|DESC])
    //
    // We distinguish by parsing the full expression and then checking whether
    // it is a bare `Expr::Column` — if so, the index targets that column
    // directly with no expression. Otherwise we keep the expression and pick
    // the first referenced column name (arbitrary but stable) as `name` so
    // catalog code that indexes by column name still works.
    let expr = parse_expr(p)?;
    let (name, expr) = match expr {
        Expr::Column { name, .. } => (name, None),
        other => {
            let col_name = first_column_name(&other)
                .map(|s| s.to_string())
                .unwrap_or_else(|| "__expr__".to_string());
            (col_name, Some(Box::new(other)))
        }
    };

    let order = if p.eat(&Token::Asc) {
        SortOrder::Asc
    } else if p.eat(&Token::Desc) {
        SortOrder::Desc
    } else {
        SortOrder::Asc
    };

    Ok(IndexColumn { name, order, expr })
}

/// Returns the name of the first `Expr::Column` found in the expression tree,
/// using a left-to-right pre-order traversal. Used by expression indexes to
/// pick a stable representative column name for catalog lookups.
fn first_column_name(expr: &Expr) -> Option<&str> {
    match expr {
        Expr::Column { name, .. } => Some(name.as_str()),
        Expr::UnaryOp { operand, .. } => first_column_name(operand),
        Expr::BinaryOp { left, right, .. } => {
            first_column_name(left).or_else(|| first_column_name(right))
        }
        Expr::IsNull { expr, .. } => first_column_name(expr),
        Expr::IsBoolean { expr, .. } => first_column_name(expr),
        Expr::Between {
            expr, low, high, ..
        } => first_column_name(expr)
            .or_else(|| first_column_name(low))
            .or_else(|| first_column_name(high)),
        Expr::Like { expr, pattern, .. } => {
            first_column_name(expr).or_else(|| first_column_name(pattern))
        }
        Expr::In { expr, list, .. } => {
            first_column_name(expr).or_else(|| list.iter().find_map(first_column_name))
        }
        Expr::Function { args, .. } => args.iter().find_map(first_column_name),
        Expr::Case {
            operand,
            when_thens,
            else_result,
        } => operand
            .as_deref()
            .and_then(first_column_name)
            .or_else(|| {
                when_thens
                    .iter()
                    .find_map(|(c, r)| first_column_name(c).or_else(|| first_column_name(r)))
            })
            .or_else(|| else_result.as_deref().and_then(first_column_name)),
        Expr::Cast { expr, .. } => first_column_name(expr),
        _ => None,
    }
}

// ── DROP TABLE ────────────────────────────────────────────────────────────────

pub(crate) fn parse_drop_schema(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let name = p.parse_identifier()?;
    let cascade = if p.eat(&Token::Cascade) {
        true
    } else {
        p.eat(&Token::Restrict); // consume optional RESTRICT; RESTRICT is the default
        false
    };
    Ok(Stmt::DropSchema(crate::ast::DropSchemaStmt {
        name,
        if_exists,
        cascade,
    }))
}

pub(crate) fn parse_drop_database(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let name = p.parse_identifier()?;
    Ok(Stmt::DropDatabase(crate::ast::DropDatabaseStmt {
        if_exists,
        name,
    }))
}

pub(crate) fn parse_drop_table(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let mut tables = vec![p.parse_table_ref()?];
    while p.eat(&Token::Comma) {
        tables.push(p.parse_table_ref()?);
    }
    let cascade = p.eat(&Token::Cascade);
    Ok(Stmt::DropTable(DropTableStmt {
        if_exists,
        tables,
        cascade,
    }))
}

pub(crate) fn parse_drop_trigger(p: &mut Parser) -> Result<Stmt, DbError> {
    let name = p.parse_identifier()?;
    p.expect(&Token::On)?;
    let table = p.parse_table_ref()?;
    Ok(Stmt::DropTrigger(DropTriggerStmt { name, table }))
}

pub(crate) fn parse_drop_aggregate(p: &mut Parser) -> Result<Stmt, DbError> {
    let name = p.parse_identifier()?;
    let arg_types = parse_aggregate_signature(p)?;
    Ok(Stmt::DropAggregate(DropAggregateStmt { name, arg_types }))
}

pub(crate) fn parse_drop_sequence(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let mut sequences = vec![p.parse_table_ref()?];
    while p.eat(&Token::Comma) {
        sequences.push(p.parse_table_ref()?);
    }
    Ok(Stmt::DropSequence(DropSequenceStmt {
        if_exists,
        sequences,
    }))
}

pub(crate) fn parse_create_view(p: &mut Parser, or_replace: bool) -> Result<Stmt, DbError> {
    let view = p.parse_table_ref()?;
    // Optional column-name list: VIEW v (a, b, c) AS ...
    let mut columns = Vec::new();
    if p.eat(&Token::LParen) {
        columns.push(p.parse_identifier()?);
        while p.eat(&Token::Comma) {
            columns.push(p.parse_identifier()?);
        }
        p.expect(&Token::RParen)?;
    }
    p.expect(&Token::As)?;
    let query_start = p.current_pos();
    p.expect(&Token::Select)?;
    let select = super::dml::parse_select(p)?;
    let query_sql = p.slice_sql(query_start, p.previous_end());
    Ok(Stmt::CreateView(CreateViewStmt {
        or_replace,
        view,
        columns,
        query_sql,
        select,
    }))
}

pub(crate) fn parse_drop_view(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let mut views = vec![p.parse_table_ref()?];
    while p.eat(&Token::Comma) {
        views.push(p.parse_table_ref()?);
    }
    Ok(Stmt::DropView(DropViewStmt { if_exists, views }))
}

pub(crate) fn parse_show_create_view(p: &mut Parser) -> Result<Stmt, DbError> {
    let view = p.parse_table_ref()?;
    Ok(Stmt::ShowCreateView(ShowCreateViewStmt { view }))
}

pub(crate) fn parse_drop_materialized_view(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let mut views = vec![p.parse_table_ref()?];
    while p.eat(&Token::Comma) {
        views.push(p.parse_table_ref()?);
    }
    let cascade = p.eat(&Token::Cascade);
    Ok(Stmt::DropMaterializedView(DropMaterializedViewStmt {
        if_exists,
        views,
        cascade,
    }))
}

pub(crate) fn parse_refresh_materialized_view(p: &mut Parser) -> Result<Stmt, DbError> {
    p.expect(&Token::Materialized)?;
    p.expect(&Token::View)?;
    let view = p.parse_table_ref()?;
    Ok(Stmt::RefreshMaterializedView(RefreshMaterializedViewStmt {
        view,
    }))
}

// ── DROP INDEX ────────────────────────────────────────────────────────────────

pub(crate) fn parse_drop_index(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let name = p.parse_identifier()?;
    let table = if p.eat(&Token::On) {
        Some(p.parse_table_ref()?)
    } else {
        None
    };
    Ok(Stmt::DropIndex(DropIndexStmt {
        if_exists,
        name,
        table,
    }))
}

// ── ALTER TABLE ───────────────────────────────────────────────────────────────

/// Parses everything after `ALTER TABLE` has been consumed.
pub(crate) fn parse_alter_table(p: &mut Parser) -> Result<Stmt, DbError> {
    let table = p.parse_table_ref()?;
    let mut operations = Vec::new();

    loop {
        let op = match p.peek().clone() {
            // ADD [CONSTRAINT name] <constraint> | ADD [COLUMN] col_def
            // ADD [UNIQUE] [INDEX|KEY] [name] (cols)
            Token::Add => {
                p.advance();
                // Constraint forms:
                // ADD CONSTRAINT name <type>
                // ADD PRIMARY KEY (...)
                // ADD FOREIGN KEY (...)
                // ADD CHECK (...)
                if matches!(
                    p.peek(),
                    Token::Constraint
                        | Token::Primary
                        | Token::Foreign
                        | Token::Check
                        | Token::Exclude
                ) {
                    let constraint = parse_table_constraint(p)?;
                    AlterTableOp::AddConstraint(constraint)
                } else if matches!(p.peek(), Token::Index | Token::Key) {
                    // ADD INDEX [name] (cols)  /  ADD KEY [name] (cols) — KEY is a synonym
                    p.advance(); // consume INDEX/KEY
                    let name = if !matches!(p.peek(), Token::LParen) {
                        Some(p.parse_identifier()?)
                    } else {
                        None
                    };
                    let columns = parse_index_column_list_paren(p)?;
                    AlterTableOp::AddIndex {
                        unique: false,
                        name,
                        columns,
                    }
                } else if matches!(p.peek(), Token::Unique) {
                    // ADD UNIQUE [INDEX|KEY] [name] (cols)  OR  ADD UNIQUE (constraint)
                    p.advance(); // consume UNIQUE
                    if matches!(p.peek(), Token::Index)
                        || matches!(p.peek(), Token::Ident(kw) if kw.eq_ignore_ascii_case("key"))
                    {
                        // ADD UNIQUE INDEX [name] (cols)
                        p.advance();
                        let name = if !matches!(p.peek(), Token::LParen) {
                            Some(p.parse_identifier()?)
                        } else {
                            None
                        };
                        let columns = parse_index_column_list_paren(p)?;
                        AlterTableOp::AddIndex {
                            unique: true,
                            name,
                            columns,
                        }
                    } else {
                        // ADD UNIQUE [name] (cols) — shorthand
                        let name = if !matches!(p.peek(), Token::LParen) {
                            Some(p.parse_identifier()?)
                        } else {
                            None
                        };
                        let columns = parse_index_column_list_paren(p)?;
                        AlterTableOp::AddIndex {
                            unique: true,
                            name,
                            columns,
                        }
                    }
                } else {
                    // ADD [COLUMN] col_def — existing behavior
                    p.eat(&Token::Column);
                    let col_def = parse_column_def(p)?;
                    AlterTableOp::AddColumn(col_def)
                }
            }
            // RENAME COLUMN old TO new  |  RENAME TO new_name  |  RENAME INDEX old TO new
            Token::Rename => {
                p.advance();
                match p.peek().clone() {
                    Token::Column => {
                        p.advance();
                        let old_name = p.parse_identifier()?;
                        p.expect(&Token::To)?;
                        let new_name = p.parse_identifier()?;
                        AlterTableOp::RenameColumn { old_name, new_name }
                    }
                    Token::To | Token::As => {
                        p.advance();
                        let new_name = p.parse_identifier()?;
                        AlterTableOp::RenameTable(new_name)
                    }
                    // RENAME INDEX old TO new
                    Token::Index => {
                        p.advance();
                        let old_name = p.parse_identifier()?;
                        p.expect(&Token::To)?;
                        let new_name = p.parse_identifier()?;
                        AlterTableOp::RenameIndex { old_name, new_name }
                    }
                    // RENAME KEY old TO new (MySQL synonym)
                    Token::Key => {
                        p.advance();
                        let old_name = p.parse_identifier()?;
                        p.expect(&Token::To)?;
                        let new_name = p.parse_identifier()?;
                        AlterTableOp::RenameIndex { old_name, new_name }
                    }
                    other => {
                        return Err(DbError::ParseError {
                            message: format!(
                            "expected COLUMN, TO, INDEX, or KEY after RENAME in ALTER TABLE, found {other:?}",
                        ),
                            position: Some(p.current_pos()),
                        })
                    }
                }
            }
            // MODIFY [COLUMN] col_name new_type [constraints]
            Token::Modify => {
                p.advance();
                p.eat(&Token::Column); // optional COLUMN keyword
                let col_def = parse_column_def(p)?;
                AlterTableOp::ModifyColumn(col_def)
            }
            // CHANGE [COLUMN] old_col_name new_col_def
            Token::Ident(kw) if kw.eq_ignore_ascii_case("change") => {
                p.advance();
                p.eat(&Token::Column); // optional COLUMN keyword
                let old_name = p.parse_identifier()?;
                let new_def = parse_column_def(p)?;
                AlterTableOp::ChangeColumn { old_name, new_def }
            }
            // DROP INDEX name | DROP PRIMARY KEY | DROP CONSTRAINT ... | DROP COLUMN ...
            Token::Drop => {
                p.advance();
                if matches!(p.peek(), Token::Index | Token::Key) {
                    p.advance();
                    let name = p.parse_identifier()?;
                    AlterTableOp::DropIndex { name }
                } else if matches!(p.peek(), Token::Primary) {
                    // DROP PRIMARY KEY — handled later in executor
                    p.advance(); // PRIMARY
                    p.eat(&Token::Key); // optional KEY keyword
                                        // Re-use DropIndex with "PRIMARY" as the sentinel name
                    AlterTableOp::DropIndex {
                        name: "PRIMARY".to_string(),
                    }
                } else if matches!(p.peek(), Token::Constraint) {
                    p.advance(); // consume CONSTRAINT
                    let if_exists =
                        if matches!(p.peek(), Token::If) && matches!(p.peek_at(1), Token::Exists) {
                            p.advance();
                            p.advance();
                            true
                        } else {
                            false
                        };
                    let name = p.parse_identifier()?;
                    AlterTableOp::DropConstraint { name, if_exists }
                } else {
                    // DROP [COLUMN] [IF EXISTS] col_name
                    p.eat(&Token::Column);
                    let if_exists =
                        if matches!(p.peek(), Token::If) && matches!(p.peek_at(1), Token::Exists) {
                            p.advance();
                            p.advance();
                            true
                        } else {
                            false
                        };
                    let name = p.parse_identifier()?;
                    AlterTableOp::DropColumn { name, if_exists }
                }
            }
            // CONVERT TO CHARACTER SET charset [COLLATE collation]
            Token::Ident(kw) if kw.eq_ignore_ascii_case("convert") => {
                p.advance();
                p.expect(&Token::To)?;
                // expect CHARACTER SET or CHARSET (either form)
                match p.peek().clone() {
                    Token::Ident(s) if s.eq_ignore_ascii_case("charset") => {
                        p.advance();
                    }
                    Token::Ident(s) if s.eq_ignore_ascii_case("character") => {
                        p.advance(); // CHARACTER
                                     // SET is Token::Set
                        p.eat(&Token::Set);
                    }
                    _ => {} // tolerate unexpected token
                }
                // consume charset name
                let _ = p.parse_identifier();
                // optional COLLATE collation
                if let Token::Ident(s) = p.peek().clone() {
                    if s.eq_ignore_ascii_case("collate") {
                        p.advance();
                        let _ = p.parse_identifier();
                    }
                }
                AlterTableOp::ConvertCharset
            }
            // AUTO_INCREMENT = N  (Token::AutoIncrement is the lexed form)
            Token::AutoIncrement => {
                p.advance();
                p.eat(&Token::Eq);
                let n = match p.peek().clone() {
                    Token::Integer(n) => {
                        p.advance();
                        n as u64
                    }
                    _ => 0,
                };
                AlterTableOp::SetAutoIncrement(n)
            }
            // ENGINE = name
            Token::Ident(kw) if kw.eq_ignore_ascii_case("engine") => {
                p.advance();
                p.eat(&Token::Eq);
                let _ = p.parse_identifier();
                AlterTableOp::SetEngine
            }
            // REBUILD — convert heap table to clustered format (Phase 39.19)
            Token::Ident(s) if s.eq_ignore_ascii_case("rebuild") => {
                p.advance();
                AlterTableOp::Rebuild
            }
            _ => break,
        };
        operations.push(op);
        if !p.eat(&Token::Comma) {
            break;
        }
    }

    if operations.is_empty() {
        return Err(DbError::ParseError {
            message: "ALTER TABLE: expected ADD, DROP, or RENAME after table name".into(),
            position: Some(p.current_pos()),
        });
    }

    Ok(Stmt::AlterTable(AlterTableStmt { table, operations }))
}

// ── FDW DDL parsers (Phase 22b.2) ─────────────────────────────────────────────

/// Parses `OPTIONS (key 'val', ...)` and returns a vec of `(key, value)` pairs.
/// Assumes the `OPTIONS` keyword has already been consumed.
fn parse_fdw_options(p: &mut Parser) -> Result<Vec<(String, String)>, DbError> {
    p.expect(&Token::LParen)?;
    let mut opts = Vec::new();
    loop {
        if matches!(p.peek(), Token::RParen | Token::Eof) {
            break;
        }
        let key = p.parse_identifier()?;
        let val = p.parse_string_literal()?;
        opts.push((key, val));
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    p.expect(&Token::RParen)?;
    Ok(opts)
}

fn fdw_datatype_to_column_type(
    dt: &axiomdb_types::DataType,
) -> Result<axiomdb_catalog::ColumnType, DbError> {
    use axiomdb_catalog::ColumnType;
    use axiomdb_types::DataType;
    match dt {
        DataType::Bool => Ok(ColumnType::Bool),
        DataType::TinyInt => Ok(ColumnType::TinyInt),
        DataType::SmallInt => Ok(ColumnType::SmallInt),
        DataType::Int => Ok(ColumnType::Int),
        DataType::BigInt => Ok(ColumnType::BigInt),
        DataType::Float => Ok(ColumnType::Float32),
        DataType::Real => Ok(ColumnType::Float),
        DataType::Text => Ok(ColumnType::Text),
        DataType::Bytes => Ok(ColumnType::Bytes),
        DataType::Timestamp => Ok(ColumnType::Timestamp),
        DataType::Uuid => Ok(ColumnType::Uuid),
        DataType::Json => Ok(ColumnType::Json),
        DataType::Jsonb => Ok(ColumnType::Jsonb),
        DataType::Decimal => Ok(ColumnType::Decimal),
        DataType::Date => Ok(ColumnType::Date),
        DataType::Array(_) => Ok(ColumnType::Array),
        DataType::Range(_) => Ok(ColumnType::Range),
        DataType::Money => Ok(ColumnType::Money),
        DataType::Composite(_) => Ok(ColumnType::Composite),
        DataType::Ltree => Ok(ColumnType::Ltree),
        DataType::Xml => Ok(ColumnType::Xml),
    }
}

/// Parses everything after `CREATE SERVER` has been consumed.
///
/// Syntax: `name [IF NOT EXISTS] FOREIGN DATA WRAPPER fdw_name [OPTIONS (key 'val', ...)]`
pub(crate) fn parse_create_server(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_not_exists = eat_if_not_exists(p)?;
    let name = p.parse_identifier()?;
    // Expect: FOREIGN DATA WRAPPER fdw_name
    p.expect(&Token::Foreign)?;
    if !p.eat_ident_ci("DATA") {
        return Err(DbError::ParseError {
            message: "expected DATA after FOREIGN in CREATE SERVER".into(),
            position: Some(p.current_pos()),
        });
    }
    if !p.eat_ident_ci("WRAPPER") {
        return Err(DbError::ParseError {
            message: "expected WRAPPER after FOREIGN DATA in CREATE SERVER".into(),
            position: Some(p.current_pos()),
        });
    }
    let fdw_name = p.parse_identifier()?;
    let options = if p.eat_ident_ci("OPTIONS") {
        parse_fdw_options(p)?
    } else {
        Vec::new()
    };
    Ok(Stmt::CreateServer(crate::ast::CreateServerStmt {
        name,
        if_not_exists,
        fdw_name,
        options,
    }))
}

/// Parses everything after `DROP SERVER` has been consumed.
///
/// Syntax: `[IF EXISTS] name`
pub(crate) fn parse_drop_server(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let name = p.parse_identifier()?;
    Ok(Stmt::DropServer(crate::ast::DropServerStmt {
        name,
        if_exists,
    }))
}

/// Parses everything after `CREATE FOREIGN TABLE` has been consumed.
///
/// Syntax: `[IF NOT EXISTS] [schema.]name (col type [NOT NULL], ...) SERVER sname [OPTIONS (...)]`
pub(crate) fn parse_create_foreign_table(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_not_exists = eat_if_not_exists(p)?;
    let table = p.parse_table_ref()?;

    p.expect(&Token::LParen)?;
    let mut columns = Vec::new();
    loop {
        if matches!(p.peek(), Token::RParen | Token::Eof) {
            break;
        }
        let col_name = p.parse_identifier()?;
        let parsed_type = parse_data_type(p)?;
        let col_type = fdw_datatype_to_column_type(&parsed_type.data_type)?;
        let nullable = !matches!(p.peek(), Token::Not) || {
            p.advance(); // consume NOT
            p.expect(&Token::Null)?;
            false
        };
        columns.push(crate::ast::FdwColumnDef {
            name: col_name,
            col_type,
            nullable,
        });
        if !p.eat(&Token::Comma) {
            break;
        }
    }
    p.expect(&Token::RParen)?;

    if !p.eat_ident_ci("SERVER") {
        return Err(DbError::ParseError {
            message: "expected SERVER after column list in CREATE FOREIGN TABLE".into(),
            position: Some(p.current_pos()),
        });
    }
    let server_name = p.parse_identifier()?;

    let options = if p.eat_ident_ci("OPTIONS") {
        parse_fdw_options(p)?
    } else {
        Vec::new()
    };

    Ok(Stmt::CreateForeignTable(
        crate::ast::CreateForeignTableStmt {
            table,
            if_not_exists,
            columns,
            server_name,
            options,
        },
    ))
}

/// Parses everything after `DROP FOREIGN TABLE` has been consumed.
///
/// Syntax: `[IF EXISTS] [schema.]name`
pub(crate) fn parse_drop_foreign_table(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let table = p.parse_table_ref()?;
    Ok(Stmt::DropForeignTable(crate::ast::DropForeignTableStmt {
        table,
        if_exists,
    }))
}

// ── Holiday calendar DDL (Phase 20.16) ────────────────────────────────────────

/// Parses everything after `CREATE HOLIDAY CALENDAR` has been consumed.
///
/// Syntax: `country_code [WITH HOLIDAYS (date_str[, ...])]`
///
/// `country_code` may be a string literal (`'CO'`) or a bare identifier (`CO`).
/// Dates are ISO-8601 string literals, e.g. `'2024-01-01'`.
pub(crate) fn parse_create_holiday_calendar(p: &mut Parser) -> Result<Stmt, DbError> {
    let country_code = parse_country_code(p)?.to_ascii_uppercase();

    let mut holidays = Vec::new();
    if matches!(p.peek(), Token::With) {
        p.advance(); // consume WITH
        if matches!(p.peek(), Token::Ident(kw) if kw.eq_ignore_ascii_case("holidays")) {
            p.advance(); // consume HOLIDAYS
        } else {
            return Err(DbError::ParseError {
                message: "expected HOLIDAYS after WITH in CREATE HOLIDAY CALENDAR".into(),
                position: Some(p.current_pos()),
            });
        }
        p.expect(&Token::LParen)?;
        if !p.eat(&Token::RParen) {
            loop {
                let date_str = match p.peek().clone() {
                    Token::StringLit(s) => {
                        p.advance();
                        s
                    }
                    other => {
                        return Err(DbError::ParseError {
                            message: format!(
                                "expected date string literal in HOLIDAYS list, found {other:?}"
                            ),
                            position: Some(p.current_pos()),
                        });
                    }
                };
                holidays.push(date_str);
                if !p.eat(&Token::Comma) {
                    break;
                }
            }
            p.expect(&Token::RParen)?;
        }
    }

    Ok(Stmt::CreateHolidayCalendar(
        crate::ast::CreateHolidayCalendarStmt {
            country_code,
            holidays,
        },
    ))
}

/// Parses everything after `DROP HOLIDAY CALENDAR` has been consumed.
///
/// Syntax: `[IF EXISTS] country_code`
pub(crate) fn parse_drop_holiday_calendar(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let country_code = parse_country_code(p)?.to_ascii_uppercase();
    Ok(Stmt::DropHolidayCalendar(
        crate::ast::DropHolidayCalendarStmt {
            if_exists,
            country_code,
        },
    ))
}

/// Parses a country code, accepting either a string literal (`'CO'`) or a
/// bare identifier (`CO`).
fn parse_country_code(p: &mut Parser) -> Result<String, DbError> {
    match p.peek().clone() {
        Token::StringLit(s) => {
            p.advance();
            Ok(s)
        }
        Token::Ident(s) => {
            let owned = s.to_string();
            p.advance();
            Ok(owned)
        }
        other => Err(DbError::ParseError {
            message: format!("expected country code string or identifier, found {other:?}"),
            position: Some(p.current_pos()),
        }),
    }
}

// ── Exchange rate DDL (Phase 20.17) ───────────────────────────────────────────

/// Parses everything after `CREATE EXCHANGE RATE` has been consumed.
///
/// Syntax: `'FROM_CURRENCY' TO 'TO_CURRENCY' rate_literal`
///
/// Example: `CREATE EXCHANGE RATE 'USD' TO 'EUR' 0.92`
pub(crate) fn parse_create_exchange_rate(p: &mut Parser) -> Result<Stmt, DbError> {
    let from_currency = parse_currency_code(p)?.to_ascii_uppercase();

    match p.peek().clone() {
        Token::To => {
            p.advance();
        }
        Token::Ident(kw) if kw.eq_ignore_ascii_case("TO") => {
            p.advance();
        }
        other => {
            return Err(DbError::ParseError {
                message: format!(
                    "expected TO after from_currency in CREATE EXCHANGE RATE, found {other:?}"
                ),
                position: Some(p.current_pos()),
            });
        }
    }

    let to_currency = parse_currency_code(p)?.to_ascii_uppercase();

    // Consume optional RATE keyword before the numeric literal
    if let Token::Ident(kw) = p.peek().clone() {
        if kw.eq_ignore_ascii_case("RATE") {
            p.advance();
        }
    }

    let rate_str = match p.peek().clone() {
        Token::Integer(n) => {
            p.advance();
            n.to_string()
        }
        Token::Float(f) => {
            p.advance();
            f.to_string()
        }
        other => {
            return Err(DbError::ParseError {
                message: format!("expected numeric rate in CREATE EXCHANGE RATE, found {other:?}"),
                position: Some(p.current_pos()),
            });
        }
    };

    Ok(Stmt::CreateExchangeRate(
        crate::ast::CreateExchangeRateStmt {
            from_currency,
            to_currency,
            rate_str,
        },
    ))
}

/// Parses everything after `DROP EXCHANGE RATE` has been consumed.
///
/// Syntax: `[IF EXISTS] 'FROM_CURRENCY' TO 'TO_CURRENCY'`
pub(crate) fn parse_drop_exchange_rate(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_exists = eat_if_exists(p)?;
    let from_currency = parse_currency_code(p)?.to_ascii_uppercase();

    match p.peek().clone() {
        Token::To => {
            p.advance();
        }
        Token::Ident(kw) if kw.eq_ignore_ascii_case("TO") => {
            p.advance();
        }
        other => {
            return Err(DbError::ParseError {
                message: format!(
                    "expected TO after from_currency in DROP EXCHANGE RATE, found {other:?}"
                ),
                position: Some(p.current_pos()),
            });
        }
    }

    let to_currency = parse_currency_code(p)?.to_ascii_uppercase();
    Ok(Stmt::DropExchangeRate(crate::ast::DropExchangeRateStmt {
        if_exists,
        from_currency,
        to_currency,
    }))
}

/// Parses a 3-character ISO 4217 currency code: string literal or bare identifier.
fn parse_currency_code(p: &mut Parser) -> Result<String, DbError> {
    match p.peek().clone() {
        Token::StringLit(s) => {
            p.advance();
            Ok(s)
        }
        Token::Ident(s) => {
            let owned = s.to_string();
            p.advance();
            Ok(owned)
        }
        other => Err(DbError::ParseError {
            message: format!("expected ISO 4217 currency code (e.g. 'USD'), found {other:?}"),
            position: Some(p.current_pos()),
        }),
    }
}
