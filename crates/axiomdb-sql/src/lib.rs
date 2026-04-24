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
pub mod clustered_secondary;
pub mod clustered_table;
pub mod eval;
pub mod exec_ctx;
pub mod executor;
pub mod expr;
pub mod expr_to_sql;
pub mod fk_enforcement;
pub mod fts_query;
pub mod index_integrity;
pub mod index_maintenance;
pub mod information_schema;
pub mod json_table;
pub mod jsonb_srf;
pub mod key_encoding;
pub mod lexer;
pub mod parser;
pub mod partial_index;
pub mod plan_deps;
pub mod planner;
pub mod recursive_cte;
pub mod result;
pub mod schema_cache;
pub mod session;
pub mod table;
pub mod text_semantics;
pub mod tokenizer;
pub mod trigram;
pub mod vacuum;
pub mod values_clause;

pub use ast::{
    AlterTableOp, AlterTableStmt, Assignment, CloseCursorStmt, ColumnConstraint, ColumnDef,
    CreateDatabaseStmt, CreateIndexStmt, CreateMaterializedViewStmt, CreateSchemaStmt,
    CreateTableStmt, CreateTriggerStmt, DeclareCursorStmt, DeleteStmt, DropDatabaseStmt,
    DropIndexStmt, DropMaterializedViewStmt, DropTableStmt, DropTriggerStmt, ExclusionElement,
    ExclusionElementTarget, ExclusionOperator, FetchCount, FetchCursorStmt, ForeignKeyAction,
    FromClause, IndexColumn, InsertSource, InsertStmt, JoinClause, JoinCondition, JoinType,
    ListenStmt, MergeAction, MergeActionCondition, MergeActionKind, MergeStmt, NotifyStmt,
    NullsOrder, OnConflictAction, OnConflictClause, OrderByItem, RefreshMaterializedViewStmt,
    SelectItem, SelectStmt, SetStmt, SetValue, ShowColumnsStmt, ShowCreateTriggerStmt,
    ShowDatabasesStmt, ShowIndexStmt, ShowTablesStmt, SortOrder, Stmt, TableConstraint, TableRef,
    TriggerEvent, TruncateTableStmt, UnlistenStmt, UpdateStmt, UseDatabaseStmt,
};
pub use bloom::BloomRegistry;
pub use eval::{
    eval, eval_in_session, eval_with, eval_with_in_session, is_truthy, like_match, ClosureRunner,
    CollationGuard, NoSubquery, SubqueryRunner,
};
pub use exec_ctx::ExecutionContext;
pub use executor::{
    cleanup_nonblocking_heap_alter_plan, commit_nonblocking_heap_alter, execute,
    execute_read_only_with_ctx, execute_snapshot, execute_with_ctx, execute_with_ctx_locked,
    prepare_nonblocking_heap_alter, truncate_table_unchecked_on_open, NonBlockingHeapAlterPlan,
};
pub use expr::{BinaryOp, Expr, UnaryOp};
pub use lexer::{tokenize, tokenize_with_sql_mode, Span, SpannedToken, Token};
pub use session::{CompatMode, SessionCollation, SqlModeFlags};
// Note: Token<'src> and SpannedToken<'src> carry a lifetime tied to the input string.
pub use analyzer::{analyze, analyze_cached, analyze_cached_with_defaults, analyze_with_defaults};
pub use axiomdb_catalog::TablePersistence;
pub use index_integrity::{verify_and_repair_indexes_on_open, IndexIntegrityReport, RebuiltIndex};
pub use parser::{parse, parse_expr_only_with_sql_mode, parse_with_sql_mode};
pub use result::{ColumnMeta, QueryResult, Row};
pub use schema_cache::SchemaCache;
pub use session::SessionContext;
pub use table::TableEngine;
