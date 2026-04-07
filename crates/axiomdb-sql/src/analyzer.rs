//! Semantic analyzer — validates references and resolves column indices.
//!
//! ## What this module does
//!
//! The parser produces `Stmt` with `Expr::Column { col_idx: 0, name }` for
//! every column reference — the `col_idx` is always a placeholder. This module:
//!
//! 1. Validates every table and column name against the catalog.
//! 2. Resolves each `col_idx` to the correct position in the **combined row**
//!    produced by the FROM + JOIN clauses.
//! 3. Reports structured errors for unknown tables, unknown columns, and
//!    ambiguous unqualified column names.
//!
//! ## Combined-row layout for JOINs
//!
//! For `FROM users u JOIN orders o ON u.id = o.user_id`:
//! ```text
//! users:   [id=0, name=1, age=2, email=3]          col_offset=0
//! orders:  [id=0, user_id=1, total=2, status=3]    col_offset=4
//! Combined: [u.id=0, u.name=1, u.age=2, u.email=3,
//!            o.id=4, o.user_id=5, o.total=6, o.status=7]
//! ```
//! `col_idx = table.col_offset + column_position_within_table`

use axiomdb_catalog::{
    schema::{ColumnDef, DEFAULT_DATABASE_NAME},
    CatalogReader,
};
use axiomdb_core::{error::DbError, TransactionSnapshot};
use axiomdb_storage::StorageEngine;

use crate::{
    ast::{
        AlterTableStmt, Assignment, ColumnConstraint, CreateIndexStmt, CreateTableStmt, DeleteStmt,
        DropTableStmt, FromClause, InsertSource, InsertStmt, JoinCondition, SelectItem, SelectStmt,
        Stmt, TableRef, UpdateStmt,
    },
    expr::Expr,
    schema_cache::SchemaCache,
};

include!("analyzer_bind.rs");
include!("analyzer_expr.rs");
include!("analyzer_stmt.rs");
include!("analyzer_ddl.rs");
