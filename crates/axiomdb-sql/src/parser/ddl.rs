//! DDL statement parsers — CREATE/DROP DATABASE, CREATE TABLE, CREATE INDEX, DROP TABLE, DROP INDEX.

use axiomdb_core::error::DbError;
use axiomdb_types::DataType;

use crate::{
    ast::{
        AlterTableOp, AlterTableStmt, ColumnConstraint, ColumnDef, CreateIndexStmt,
        CreateTableStmt, DropIndexStmt, DropTableStmt, ForeignKeyAction, IndexColumn, SortOrder,
        Stmt, TableConstraint,
    },
    lexer::Token,
};

use super::{expr::parse_expr, Parser};

// ── CREATE TABLE ──────────────────────────────────────────────────────────────

pub(crate) fn parse_create_database(p: &mut Parser) -> Result<Stmt, DbError> {
    let name = p.parse_identifier()?;
    Ok(Stmt::CreateDatabase(crate::ast::CreateDatabaseStmt {
        name,
    }))
}

pub(crate) fn parse_create_schema(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_not_exists = eat_if_not_exists(p)?;
    let name = p.parse_identifier()?;
    Ok(Stmt::CreateSchema(crate::ast::CreateSchemaStmt {
        name,
        if_not_exists,
    }))
}

/// Parses everything after `CREATE TABLE` has been consumed.
pub(crate) fn parse_create_table(p: &mut Parser) -> Result<Stmt, DbError> {
    let if_not_exists = eat_if_not_exists(p)?;
    let table = p.parse_table_ref()?;
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

    Ok(Stmt::CreateTable(CreateTableStmt {
        if_not_exists,
        table,
        columns,
        table_constraints,
    }))
}

fn is_table_constraint_start(p: &Parser) -> bool {
    matches!(
        p.peek(),
        Token::Primary | Token::Unique | Token::Foreign | Token::Check | Token::Constraint
    )
}

// ── Column definition ─────────────────────────────────────────────────────────

fn parse_column_def(p: &mut Parser) -> Result<ColumnDef, DbError> {
    let name = p.parse_identifier()?;
    let data_type = parse_data_type(p)?;
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
            _ => break,
        }
    }

    Ok(ColumnDef {
        name,
        data_type,
        constraints,
    })
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
        other => Err(DbError::ParseError {
            message: format!(
                "expected PRIMARY, UNIQUE, FOREIGN, or CHECK in table constraint, found {:?}",
                other,
            ),
            position: Some(p.current_pos()),
        }),
    }
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

// ── CHECK (column-level) ──────────────────────────────────────────────────────

fn parse_check_column_constraint(p: &mut Parser) -> Result<ColumnConstraint, DbError> {
    p.advance(); // consume CHECK
    p.expect(&Token::LParen)?;
    let expr = parse_expr(p)?;
    p.expect(&Token::RParen)?;
    Ok(ColumnConstraint::Check(expr))
}

// ── Data type ─────────────────────────────────────────────────────────────────

pub(crate) fn parse_data_type(p: &mut Parser) -> Result<DataType, DbError> {
    let pos = p.current_pos();
    match p.peek().clone() {
        Token::TyInt | Token::TyInteger => {
            p.advance();
            Ok(DataType::Int)
        }
        Token::TyBigint => {
            p.advance();
            Ok(DataType::BigInt)
        }
        Token::TyReal | Token::TyDouble | Token::TyFloat => {
            p.advance();
            Ok(DataType::Real)
        }
        Token::TyDecimal | Token::TyNumeric => {
            p.advance();
            eat_optional_precision_scale(p)?;
            Ok(DataType::Decimal)
        }
        Token::TyBool | Token::TyBoolean => {
            p.advance();
            Ok(DataType::Bool)
        }
        Token::TyText => {
            p.advance();
            Ok(DataType::Text)
        }
        Token::TyVarchar | Token::TyChar => {
            p.advance();
            eat_optional_length(p)?;
            Ok(DataType::Text)
        }
        Token::TyBlob | Token::TyBytea => {
            p.advance();
            Ok(DataType::Bytes)
        }
        Token::TyDate => {
            p.advance();
            Ok(DataType::Date)
        }
        Token::TyTimestamp | Token::TyDatetime => {
            p.advance();
            Ok(DataType::Timestamp)
        }
        Token::TyUuid => {
            p.advance();
            Ok(DataType::Uuid)
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

fn eat_optional_length(p: &mut Parser) -> Result<(), DbError> {
    if p.eat(&Token::LParen) {
        if !matches!(p.peek(), Token::Integer(_)) {
            return Err(DbError::ParseError {
                message: "expected length integer in type parameter".into(),
                position: Some(p.current_pos()),
            });
        }
        p.advance();
        p.expect(&Token::RParen)?;
    }
    Ok(())
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

    // Optional WITH (key = value, ...) storage options (Phase 6.8).
    // Only `fillfactor` is supported; other keys return ParseError.
    let fillfactor: Option<u8> = if p.eat(&Token::With) {
        p.expect(&Token::LParen)?;
        let key = p.parse_identifier()?;
        p.expect(&Token::Eq)?;
        let ff = match key.to_lowercase().as_str() {
            "fillfactor" => {
                let val = match p.peek() {
                    Token::Integer(n) => {
                        let n = *n;
                        p.advance();
                        n
                    }
                    other => {
                        return Err(DbError::ParseError {
                            message: format!("fillfactor must be an integer, found {:?}", other),
                            position: Some(p.current_pos()),
                        });
                    }
                };
                if !(10..=100).contains(&val) {
                    return Err(DbError::ParseError {
                        message: "fillfactor must be between 10 and 100".into(),
                        position: Some(p.current_pos()),
                    });
                }
                val as u8
            }
            other => {
                return Err(DbError::ParseError {
                    message: format!("unknown index option: {other}"),
                    position: Some(p.current_pos()),
                });
            }
        };
        // Allow trailing comma before closing paren.
        p.eat(&Token::Comma);
        p.expect(&Token::RParen)?;
        Some(ff)
    } else {
        None
    };

    Ok(Stmt::CreateIndex(CreateIndexStmt {
        if_not_exists,
        unique,
        name,
        table,
        columns,
        include_columns,
        predicate,
        fillfactor,
    }))
}

fn parse_index_column(p: &mut Parser) -> Result<IndexColumn, DbError> {
    let name = p.parse_identifier()?;
    let order = if p.eat(&Token::Asc) {
        SortOrder::Asc
    } else if p.eat(&Token::Desc) {
        SortOrder::Desc
    } else {
        SortOrder::Asc
    };
    Ok(IndexColumn { name, order })
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
            Token::Add => {
                p.advance();
                // Peek: is this ADD CONSTRAINT or ADD UNIQUE (without CONSTRAINT keyword)?
                if matches!(p.peek(), Token::Constraint) {
                    // ADD CONSTRAINT name <type>
                    // Do NOT pre-consume CONSTRAINT — parse_table_constraint
                    // handles the optional CONSTRAINT name prefix itself.
                    let constraint = parse_table_constraint(p)?;
                    AlterTableOp::AddConstraint(constraint)
                } else if matches!(p.peek(), Token::Unique) {
                    // ADD UNIQUE (cols) — shorthand without CONSTRAINT name
                    let constraint = parse_table_constraint(p)?; // eats Unique
                    AlterTableOp::AddConstraint(constraint)
                } else {
                    // ADD [COLUMN] col_def — existing behavior
                    p.eat(&Token::Column);
                    let col_def = parse_column_def(p)?;
                    AlterTableOp::AddColumn(col_def)
                }
            }
            // DROP CONSTRAINT [IF EXISTS] name | DROP [COLUMN] [IF EXISTS] col_name
            Token::Drop => {
                p.advance();
                if matches!(p.peek(), Token::Constraint) {
                    // DROP CONSTRAINT [IF EXISTS] name
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
                    // DROP [COLUMN] [IF EXISTS] col_name — existing behavior
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
            // RENAME COLUMN old TO new  |  RENAME TO new_name
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
                    Token::To => {
                        p.advance();
                        let new_name = p.parse_identifier()?;
                        AlterTableOp::RenameTable(new_name)
                    }
                    other => {
                        return Err(DbError::ParseError {
                            message: format!(
                            "expected COLUMN or TO after RENAME in ALTER TABLE, found {other:?}",
                        ),
                            position: Some(p.current_pos()),
                        })
                    }
                }
            }
            // MODIFY [COLUMN] col_def — not yet implemented
            Token::Modify => {
                return Err(DbError::NotImplemented {
                    feature: "ALTER TABLE MODIFY COLUMN — Phase N".into(),
                });
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
