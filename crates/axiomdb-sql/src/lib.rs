//! # axiomdb-sql — SQL AST, expression tree, evaluator, lexer, parser, and executor
//!
//! - 4.17: [`Expr`], [`eval`], [`is_truthy`] — expression evaluator with full NULL semantics
//! - 4.1:  [`Stmt`] and all statement AST types
//! - 4.2:  [`Token`], [`tokenize`], [`Span`], [`SpannedToken`] — SQL lexer
//! - 4.3–4.4: [`parse`] — recursive descent SQL parser
//! - 4.18: [`analyze`] — semantic analyzer, col_idx resolution
//! - 4.23: [`QueryResult`], [`ColumnMeta`], [`Row`] — unified executor return type
//! - 4.5:  [`execute`] — basic executor (SELECT, INSERT, UPDATE, DELETE, DDL, txn control)

pub mod analyzer;
pub mod ast;
pub mod bloom;
pub mod eval;
pub mod executor;
pub mod expr;
pub mod fk_enforcement;
pub mod index_maintenance;
pub mod key_encoding;
pub mod lexer;
pub mod parser;
pub mod partial_index;
pub mod planner;
pub mod result;
pub mod schema_cache;
pub mod session;
pub mod table;
pub mod text_semantics;

pub use ast::{
    AlterTableOp, AlterTableStmt, Assignment, ColumnConstraint, ColumnDef, CreateIndexStmt,
    CreateTableStmt, DeleteStmt, DropIndexStmt, DropTableStmt, ForeignKeyAction, FromClause,
    IndexColumn, InsertSource, InsertStmt, JoinClause, JoinCondition, JoinType, NullsOrder,
    OrderByItem, SelectItem, SelectStmt, SetStmt, SetValue, ShowColumnsStmt, ShowTablesStmt,
    SortOrder, Stmt, TableConstraint, TableRef, TruncateTableStmt, UpdateStmt,
};
pub use bloom::BloomRegistry;
pub use eval::{
    eval, eval_in_session, eval_with, eval_with_in_session, is_truthy, like_match, ClosureRunner,
    CollationGuard, NoSubquery, SubqueryRunner,
};
pub use executor::{execute, execute_with_ctx};
pub use expr::{BinaryOp, Expr, UnaryOp};
pub use lexer::{tokenize, Span, SpannedToken, Token};
pub use session::{CompatMode, SessionCollation};
// Note: Token<'src> and SpannedToken<'src> carry a lifetime tied to the input string.
pub use analyzer::{analyze, analyze_cached};
pub use parser::parse;
pub use result::{ColumnMeta, QueryResult, Row};
pub use schema_cache::SchemaCache;
pub use session::SessionContext;
pub use table::TableEngine;
