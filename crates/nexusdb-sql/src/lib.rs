//! # nexusdb-sql — SQL AST, expression tree, evaluator, lexer, parser, and executor
//!
//! - 4.17: [`Expr`], [`eval`], [`is_truthy`] — expression evaluator with full NULL semantics
//! - 4.1:  [`Stmt`] and all statement AST types
//! - 4.2:  [`Token`], [`tokenize`], [`Span`], [`SpannedToken`] — SQL lexer
//! - 4.3–4.4: SQL parser (coming)
//! - 4.18: Semantic analyzer (coming)
//! - 4.5:  Executor (coming)

pub mod ast;
pub mod eval;
pub mod expr;
pub mod lexer;

pub use ast::{
    AlterTableOp, AlterTableStmt, Assignment, ColumnConstraint, ColumnDef, CreateIndexStmt,
    CreateTableStmt, DeleteStmt, DropIndexStmt, DropTableStmt, ForeignKeyAction, FromClause,
    IndexColumn, InsertSource, InsertStmt, JoinClause, JoinCondition, JoinType, NullsOrder,
    OrderByItem, SelectItem, SelectStmt, SetStmt, SetValue, ShowColumnsStmt, ShowTablesStmt,
    SortOrder, Stmt, TableConstraint, TableRef, TruncateTableStmt, UpdateStmt,
};
pub use eval::{eval, is_truthy};
pub use expr::{BinaryOp, Expr, UnaryOp};
pub use lexer::{tokenize, Span, SpannedToken, Token};
