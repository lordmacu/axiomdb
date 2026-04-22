//! DDL statement parsers — CREATE/DROP DATABASE, CREATE TABLE, CREATE INDEX, DROP TABLE, DROP INDEX.

use axiomdb_catalog::TablePersistence;
use axiomdb_core::error::DbError;
use axiomdb_types::DataType;

use crate::{
    ast::{
        AlterTableOp, AlterTableStmt, ColumnConstraint, ColumnDef, CreateIndexStmt,
        CreateTableAsSelectStmt, CreateTableLikeStmt, CreateTableStmt, DropIndexStmt,
        DropTableStmt, ExclusionElement, ExclusionElementTarget, ExclusionOperator,
        ForeignKeyAction, GeneratedColumnKind, IndexColumn, SortOrder, Stmt, TableConstraint,
    },
    expr::Expr,
    lexer::Token,
};

use super::{expr::parse_expr, Parser};

// ── CREATE TABLE ──────────────────────────────────────────────────────────────

pub(crate) fn parse_create_database(p: &mut Parser) -> Result<Stmt, DbError> {
    // Optional IF NOT EXISTS
    eat_if_not_exists(p)?;
    let name = p.parse_identifier()?;
    // Consume optional CHARACTER SET / COLLATE / DEFAULT modifiers.
    // mysqldump generates: CREATE DATABASE IF NOT EXISTS `db` DEFAULT CHARACTER SET utf8mb4
    skip_create_database_options(p);
    Ok(Stmt::CreateDatabase(crate::ast::CreateDatabaseStmt {
        name,
    }))
}

/// Consume optional CHARACTER SET / COLLATE / DEFAULT clauses after a database name.
fn skip_create_database_options(p: &mut Parser) {
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
                let _ = p.parse_identifier();
            }
            _ => break,
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
    skip_table_options(p);

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
        immutable,
        persistence,
    }))
}

/// Consume MySQL table-level options that appear after the closing `)` of a
/// CREATE TABLE statement. Examples:
///   `ENGINE=InnoDB DEFAULT CHARSET=utf8mb4 COLLATE=utf8mb4_unicode_ci AUTO_INCREMENT=5`
///
/// These are all silently discarded — AxiomDB doesn't use them but must not
/// fail to parse valid MySQL schema dumps.
fn skip_table_options(p: &mut Parser) {
    loop {
        // Optional DEFAULT before CHARSET / CHARACTER SET / COLLATE
        let had_default = p.eat(&Token::Default);
        match p.peek().clone() {
            // key = value pairs: ENGINE=InnoDB, AUTO_INCREMENT=5, etc.
            // Identifiers that are MySQL-specific option keywords
            Token::Ident(s)
                if s.eq_ignore_ascii_case("engine")
                    || s.eq_ignore_ascii_case("charset")
                    || s.eq_ignore_ascii_case("collate")
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
            _ if had_default => break,
            _ => break,
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
    let (data_type, type_len, is_char) = parse_data_type(p)?;
    let mut constraints = Vec::new();

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
                let _ = p.parse_identifier()?;
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
        constraints,
        type_len,
        is_char,
    })
}

fn parse_generated_column_constraint(p: &mut Parser) -> Result<ColumnConstraint, DbError> {
    p.advance(); // GENERATED
    if !p.eat_ident_ci("always") {
        return Err(DbError::ParseError {
            message: "expected ALWAYS after GENERATED".into(),
            position: Some(p.current_pos()),
        });
    }
    p.expect(&Token::As)?;
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

/// Parses a SQL data type keyword, returning `(DataType, type_len, is_char)`.
///
/// `type_len` is the `N` from `VARCHAR(N)` / `CHAR(N)`, or `0` for all other types.
/// `is_char` is `true` only for `CHAR(N)` declarations (fixed-length).
pub(crate) fn parse_data_type(p: &mut Parser) -> Result<(DataType, u16, bool), DbError> {
    let pos = p.current_pos();
    match p.peek().clone() {
        Token::TyInt | Token::TyInteger => {
            p.advance();
            Ok((DataType::Int, 0, false))
        }
        Token::TyBigint => {
            p.advance();
            Ok((DataType::BigInt, 0, false))
        }
        Token::TyReal | Token::TyDouble | Token::TyFloat => {
            p.advance();
            Ok((DataType::Real, 0, false))
        }
        Token::TyDecimal | Token::TyNumeric => {
            p.advance();
            eat_optional_precision_scale(p)?;
            Ok((DataType::Decimal, 0, false))
        }
        Token::TyBool | Token::TyBoolean => {
            p.advance();
            Ok((DataType::Bool, 0, false))
        }
        Token::TyText => {
            p.advance();
            Ok((DataType::Text, 0, false))
        }
        Token::TyVarchar => {
            p.advance();
            let len = eat_optional_length(p)?;
            Ok((DataType::Text, len, false))
        }
        Token::TyChar => {
            p.advance();
            let len = eat_optional_length(p)?;
            Ok((DataType::Text, len, true))
        }
        Token::TyBlob | Token::TyBytea => {
            p.advance();
            Ok((DataType::Bytes, 0, false))
        }
        Token::TyDate => {
            p.advance();
            Ok((DataType::Date, 0, false))
        }
        Token::TyTimestamp | Token::TyDatetime => {
            p.advance();
            Ok((DataType::Timestamp, 0, false))
        }
        Token::TyUuid => {
            p.advance();
            Ok((DataType::Uuid, 0, false))
        }
        Token::TyJson => {
            p.advance();
            Ok((DataType::Json, 0, false))
        }
        Token::TyJsonb => {
            p.advance();
            Ok((DataType::Jsonb, 0, false))
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("TINYINT") => {
            p.advance();
            eat_optional_length(p)?; // TINYINT(N) — display width, ignored
            Ok((DataType::Bool, 0, false))
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("SMALLINT") => {
            p.advance();
            eat_optional_length(p)?;
            Ok((DataType::Int, 0, false))
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("MEDIUMINT") => {
            p.advance();
            eat_optional_length(p)?;
            Ok((DataType::Int, 0, false))
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("YEAR") => {
            p.advance();
            eat_optional_length(p)?; // YEAR(4)
            Ok((DataType::Int, 0, false))
        }
        Token::Ident(s) if s.eq_ignore_ascii_case("TIME") => {
            p.advance();
            Ok((DataType::Timestamp, 0, false))
        }
        other => Err(DbError::ParseError {
            message: format!(
                "expected a data type (INT, TEXT, BIGINT, …) but found {:?}",
                other,
            ),
            position: Some(pos),
        }),
    }
}

fn eat_optional_precision_scale(p: &mut Parser) -> Result<(), DbError> {
    if p.eat(&Token::LParen) {
        if !matches!(p.peek(), Token::Integer(_)) {
            return Err(DbError::ParseError {
                message: "expected precision integer in type parameters".into(),
                position: Some(p.current_pos()),
            });
        }
        p.advance();
        if p.eat(&Token::Comma) {
            if !matches!(p.peek(), Token::Integer(_)) {
                return Err(DbError::ParseError {
                    message: "expected scale integer after comma in type parameters".into(),
                    position: Some(p.current_pos()),
                });
            }
            p.advance();
        }
        p.expect(&Token::RParen)?;
    }
    Ok(())
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
