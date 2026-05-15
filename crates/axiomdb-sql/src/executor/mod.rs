//! Basic SQL executor — interprets an analyzed [`Stmt`] and produces a [`QueryResult`].
//!
//! ## Entry point
//!
//! [`execute`] is the single public function. It accepts an analyzed statement
//! (all `col_idx` resolved by the semantic analyzer) and drives it to completion,
//! returning a [`QueryResult`].
//!
//! ## Autocommit
//!
//! If no transaction is active when `execute` is called, the statement is
//! automatically wrapped in an implicit `BEGIN / COMMIT` via
//! [`TxnManager::autocommit`]. Transaction control statements (`BEGIN`,
//! `COMMIT`, `ROLLBACK`) bypass autocommit and operate on the `TxnManager`
//! directly.
//!
//! ## Snapshot selection
//!
//! All reads inside a statement use [`TxnManager::active_snapshot`] so that
//! writes made earlier in the same transaction are visible (read-your-own-writes).
//! This is always valid because:
//! - In autocommit mode, `autocommit()` calls `begin()` before invoking the handler.
//! - In explicit transaction mode, `begin()` was already called by the user.
//!
//! ## Phase 4.5 scope
//!
//! Supported: SELECT (with optional WHERE + projection), SELECT without FROM,
//! INSERT VALUES, UPDATE, DELETE, CREATE TABLE, DROP TABLE, CREATE INDEX,
//! DROP INDEX, BEGIN / COMMIT / ROLLBACK, SET (stub).
//!
//! Not yet supported (returns [`DbError::NotImplemented`]):
//! JOIN, GROUP BY, ORDER BY, LIMIT, DISTINCT, subqueries in FROM, INSERT SELECT,
//! TRUNCATE, ALTER TABLE, SHOW TABLES / DESCRIBE.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::collections::HashMap as StdHashMap;

use axiomdb_catalog::{
    schema::{
        ColumnDef as CatalogColumnDef, ColumnType, IndexColumnDef, IndexDef,
        SortOrder as CatalogSortOrder, TableDef, DEFAULT_DATABASE_NAME,
    },
    CatalogReader, CatalogWriter, ForeignColumnDef, ForeignServerDef, ForeignTableDef,
    ResolvedTable, SchemaResolver, FOREIGN_TABLE_ID_BASE,
};
use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot};
use axiomdb_index::{page_layout::encode_rid, BTree};
use axiomdb_storage::{
    heap_chain::{chain_next_page, HeapChain},
    Page, PageType, StorageEngine,
};
use axiomdb_types::{DataType, Value};
use axiomdb_wal::{ConnectionTxn, IndexUndoRecord, Savepoint, TxnManager};

use crate::exec_ctx::ExecutionContext;
use crate::{
    ast::{
        AlterTableOp, AlterTableStmt, Assignment, ColumnConstraint, CreateAggregateStmt,
        CreateDatabaseStmt, CreateIndexStmt, CreateTableStmt, CreateTriggerStmt, DeleteStmt,
        DropAggregateStmt, DropDatabaseStmt, DropIndexStmt, DropTableStmt, DropTriggerStmt,
        FromClause, GeneratedColumnKind, GroupByClause, InsertSource, InsertStmt, IntoOutfile,
        JoinClause, JoinCondition, JoinType, LockStrength, LockWaitPolicy, MergeActionCondition,
        MergeActionKind, MergeStmt, NullsOrder, OnConflictAction, OrderByItem, SelectItem,
        SelectStmt, SetOpKind, SetOpTail, SetStmt, SetValue, ShowCreateTriggerStmt,
        ShowDatabasesStmt, SortOrder, Stmt, TableRef, TriggerEvent, UpdateStmt, UseDatabaseStmt,
    },
    eval::{eval, eval_with, is_truthy, CollationGuard, InSubquerySet, SubqueryRunner},
    expr::{BinaryOp, Expr},
    result::{ColumnMeta, QueryResult, Row},
    session::{
        normalize_sql_mode, parse_boolish_setting, parse_compat_mode_setting,
        parse_on_error_setting, parse_session_collation_setting, sql_mode_is_strict, OnErrorMode,
        SessionCollation, SessionContext, SessionSavepoint,
    },
    table::TableEngine,
    text_semantics::compare_text,
};

/// Inline FK spec collected during CREATE TABLE column processing.
/// `(child_col_idx, child_col_name, (parent_table, parent_col, on_delete, on_update))`
type InlineFkSpec = (
    u16,
    String,
    (
        String,
        Option<String>,
        crate::ast::ForeignKeyAction,
        crate::ast::ForeignKeyAction,
        crate::ast::ConstraintDeferrability,
    ),
);

include!("exec_subquery.rs");
include!("sequence_runtime.rs");
include!("cron_runtime.rs");
include!("exec_entry.rs");
include!("exec_with_ctx.rs");
include!("exec_dispatch.rs");
include!("exec_explain.rs");
include!("cursor.rs");

include!("shared.rs");
include!("joins.rs");
include!("dml_join.rs");
include!("aggregate.rs");
include!("select.rs");
include!("insert.rs");
include!("replace_helpers.rs");
include!("on_conflict_helpers.rs");
include!("odku_helpers.rs");
include!("merge.rs");
include!("update_ctx.rs");
include!("update_fused_range.rs");
include!("update_clustered_helpers.rs");
include!("update_clustered.rs");
include!("update_candidates.rs");
include!("update_entry.rs");
include!("bulk_empty.rs");
include!("delete.rs");
include!("returning.rs");
include!("recursive_cte_exec.rs");
include!("ddl_create_table.rs");
include!("ddl_aggregate.rs");
include!("ddl_sequence.rs");
include!("ddl_view.rs");
include!("trigger.rs");
include!("ddl_drop_table.rs");
include!("ddl_create_index.rs");
include!("ddl_drop_index.rs");
include!("ddl_analyze.rs");
include!("ddl_fdw.rs");
include!("fdw_http.rs");
include!("copy_from.rs");
include!("copy_to.rs");
include!("parquet_read.rs");
include!("select_into_outfile.rs");
include!("ddl_show.rs");
include!("ddl_alter_column.rs");
include!("ddl_alter_constraint.rs");
include!("ddl_alter_rebuild.rs");
include!("staging.rs");
include!("union.rs");

/// Startup-only helper used by the server layer to empty UNLOGGED tables after
/// a dirty open. This bypasses user-facing TRUNCATE semantics (FK checks,
/// immutable-table checks, affected-row reporting) and performs the same root
/// rotation directly under an internal transaction.
pub fn truncate_table_unchecked_on_open(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    table: &ResolvedTable,
) -> Result<(), DbError> {
    let mut conn_txn = txn.begin()?;
    let snap = txn.active_snapshot(&conn_txn);
    let all_indexes: Vec<axiomdb_catalog::IndexDef> = table
        .indexes
        .iter()
        .filter(|i| !i.columns.is_empty())
        .cloned()
        .collect();
    let noop_bloom = crate::bloom::BloomRegistry::new();
    let plan = if table.def.is_clustered() {
        plan_bulk_empty_clustered_table(storage, &table.def, &all_indexes, snap)?
    } else {
        plan_bulk_empty_table(storage, &table.def, &all_indexes, snap)?
    };

    let result = apply_bulk_empty_table(storage, txn, &mut conn_txn, &noop_bloom, &table.def, plan);
    match result {
        Ok(()) => {
            let tid = conn_txn.txn_id;
            let _ = txn.commit(conn_txn)?;
            txn.release_immediate_committed_frees(storage, tid)?;
            txn.drain_committed_page_batches(storage)?;
            AUTO_INC_SEQ.with(|seq| {
                seq.borrow_mut().remove(&table.def.id);
            });
            Ok(())
        }
        Err(e) => {
            let _ = rollback_with_index_undo(txn, conn_txn, storage, &noop_bloom);
            Err(e)
        }
    }
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_datatype_to_column_type_supported() {
        assert_eq!(
            datatype_to_column_type(&DataType::Bool).unwrap(),
            ColumnType::Bool
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Int).unwrap(),
            ColumnType::Int
        );
        assert_eq!(
            datatype_to_column_type(&DataType::BigInt).unwrap(),
            ColumnType::BigInt
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Real).unwrap(),
            ColumnType::Float
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Text).unwrap(),
            ColumnType::Text
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Bytes).unwrap(),
            ColumnType::Bytes
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Timestamp).unwrap(),
            ColumnType::Timestamp
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Uuid).unwrap(),
            ColumnType::Uuid
        );

        assert_eq!(
            datatype_to_column_type(&DataType::Decimal).unwrap(),
            ColumnType::Decimal
        );
        assert_eq!(
            datatype_to_column_type(&DataType::Date).unwrap(),
            ColumnType::Date
        );
    }

    #[test]
    fn test_column_type_to_datatype_roundtrip() {
        for dt in &[
            DataType::Bool,
            DataType::Int,
            DataType::BigInt,
            DataType::Real,
            DataType::Decimal,
            DataType::Text,
            DataType::Bytes,
            DataType::Date,
            DataType::Timestamp,
            DataType::Uuid,
        ] {
            let ct = datatype_to_column_type(&dt).unwrap();
            assert_eq!(column_type_to_datatype(ct), dt.clone());
        }
    }

    #[test]
    fn test_expr_column_name_alias_wins() {
        let expr = Expr::Literal(Value::Int(1));
        assert_eq!(expr_column_name(&expr, Some("total")), "total");
    }

    #[test]
    fn test_expr_column_name_column_expr() {
        let expr = Expr::Column {
            name: "age".into(),
            col_idx: 0,
        };
        assert_eq!(expr_column_name(&expr, None), "age");
    }

    #[test]
    fn test_expr_column_name_other_expr_fallback() {
        let expr = Expr::Literal(Value::Int(1));
        assert_eq!(expr_column_name(&expr, None), "?column?");
    }

    fn make_index_def(col_idxs: &[u16]) -> IndexDef {
        use axiomdb_catalog::schema::{IndexColumnDef, SortOrder};
        IndexDef {
            index_id: 1,
            table_id: 1,
            name: "test_idx".into(),
            root_page_id: 1,
            is_unique: false,
            is_primary: false,
            columns: col_idxs
                .iter()
                .map(|&c| IndexColumnDef {
                    col_idx: c,
                    order: SortOrder::Asc,
                    expr: None,
                })
                .collect(),
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
        }
    }

    fn col_expr(idx: usize) -> Expr {
        Expr::Column {
            col_idx: idx,
            name: format!("c{idx}"),
        }
    }

    #[test]
    fn test_group_by_matches_index_prefix_single_col() {
        let idx = make_index_def(&[2]);
        assert!(group_by_matches_index_prefix(&[col_expr(2)], &idx));
    }

    #[test]
    fn test_group_by_matches_index_prefix_composite_full() {
        let idx = make_index_def(&[1, 3]);
        assert!(group_by_matches_index_prefix(
            &[col_expr(1), col_expr(3)],
            &idx
        ));
    }

    #[test]
    fn test_group_by_matches_index_prefix_leading_only() {
        let idx = make_index_def(&[1, 3]);
        assert!(group_by_matches_index_prefix(&[col_expr(1)], &idx));
    }

    #[test]
    fn test_group_by_matches_index_prefix_reordered_fails() {
        let idx = make_index_def(&[1, 3]);
        assert!(!group_by_matches_index_prefix(
            &[col_expr(3), col_expr(1)],
            &idx
        ));
    }

    #[test]
    fn test_group_by_matches_index_prefix_non_column_expr_fails() {
        let idx = make_index_def(&[2]);
        let lower_expr = Expr::Function {
            name: "lower".into(),
            args: vec![col_expr(2)],
        };
        assert!(!group_by_matches_index_prefix(&[lower_expr], &idx));
    }

    #[test]
    fn test_group_by_matches_index_prefix_empty_group_by() {
        let idx = make_index_def(&[2]);
        assert!(group_by_matches_index_prefix(&[], &idx));
    }

    #[test]
    fn test_group_by_matches_index_prefix_longer_than_index_fails() {
        let idx = make_index_def(&[1]);
        assert!(!group_by_matches_index_prefix(
            &[col_expr(1), col_expr(2)],
            &idx
        ));
    }

    #[test]
    fn test_group_keys_equal_nulls() {
        assert!(group_keys_equal(&[Value::Null], &[Value::Null]));
    }

    #[test]
    fn test_group_keys_equal_mixed() {
        assert!(group_keys_equal(
            &[Value::Int(1), Value::Text("a".into())],
            &[Value::Int(1), Value::Text("a".into())]
        ));
        assert!(!group_keys_equal(
            &[Value::Int(1), Value::Text("a".into())],
            &[Value::Int(1), Value::Text("b".into())]
        ));
    }

    #[test]
    fn test_compare_group_key_lists_ordering() {
        use std::cmp::Ordering;
        assert_eq!(
            compare_group_key_lists(&[Value::Int(1)], &[Value::Int(2)]),
            Ordering::Less
        );
        assert_eq!(
            compare_group_key_lists(&[Value::Int(2)], &[Value::Int(1)]),
            Ordering::Greater
        );
        assert_eq!(
            compare_group_key_lists(&[Value::Null], &[Value::Int(1)]),
            Ordering::Greater
        );
    }
}
