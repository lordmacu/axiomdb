use std::ops::Bound;

use axiomdb_catalog::{ColumnDef, IndexDef};
use axiomdb_core::{error::DbError, TransactionSnapshot};
use axiomdb_storage::{clustered_tree, StorageEngine};
use axiomdb_types::{codec::encode_row, Value};

use crate::{
    key_encoding::encode_index_key,
    session::SessionContext,
    table::{coerce_values, coerce_values_with_ctx, column_data_types, decode_row_from_bytes},
};

#[derive(Debug, Clone)]
pub(crate) struct PreparedClusteredInsertRow {
    pub values: Vec<Value>,
    pub encoded_row: Vec<u8>,
    pub primary_key_values: Vec<Value>,
    pub primary_key_bytes: Vec<u8>,
}

pub(crate) fn primary_index<'a>(
    indexes: &'a [IndexDef],
    table_name: &str,
) -> Result<&'a IndexDef, DbError> {
    indexes
        .iter()
        .find(|idx| idx.is_primary && !idx.columns.is_empty())
        .ok_or_else(|| DbError::InvalidValue {
            reason: format!("clustered table '{table_name}' is missing primary index metadata"),
        })
}

pub(crate) fn prepare_row(
    values: Vec<Value>,
    columns: &[ColumnDef],
    primary_idx: &IndexDef,
    table_name: &str,
) -> Result<PreparedClusteredInsertRow, DbError> {
    let coerced = coerce_values(values, columns)?;
    encode_prepared_row(coerced, columns, primary_idx, table_name)
}

pub(crate) fn prepare_row_with_ctx(
    values: Vec<Value>,
    columns: &[ColumnDef],
    primary_idx: &IndexDef,
    table_name: &str,
    ctx: &mut SessionContext,
    row_num: usize,
) -> Result<PreparedClusteredInsertRow, DbError> {
    let coerced = coerce_values_with_ctx(values, columns, ctx, row_num)?;
    encode_prepared_row(coerced, columns, primary_idx, table_name)
}

pub(crate) fn scan_max_numeric_column(
    storage: &dyn StorageEngine,
    root_pid: Option<u64>,
    columns: &[ColumnDef],
    col_idx: usize,
    snapshot: &TransactionSnapshot,
) -> Result<u64, DbError> {
    let rows = clustered_tree::range(
        storage,
        root_pid,
        Bound::Unbounded,
        Bound::Unbounded,
        snapshot,
    )?;

    let mut max_existing = 0u64;
    for row in rows {
        let row = row?;
        let decoded = decode_row_from_bytes(&row.row_data, columns)?;
        match decoded.get(col_idx) {
            Some(Value::Int(n)) => max_existing = max_existing.max(*n as u64),
            Some(Value::BigInt(n)) => max_existing = max_existing.max(*n as u64),
            _ => {}
        }
    }

    Ok(max_existing)
}

fn encode_prepared_row(
    values: Vec<Value>,
    columns: &[ColumnDef],
    primary_idx: &IndexDef,
    table_name: &str,
) -> Result<PreparedClusteredInsertRow, DbError> {
    let primary_key_values = collect_primary_key_values(&values, columns, primary_idx, table_name)?;
    let primary_key_bytes = encode_index_key(&primary_key_values)?;
    let encoded_row = encode_row(&values, &column_data_types(columns))?;

    Ok(PreparedClusteredInsertRow {
        values,
        encoded_row,
        primary_key_values,
        primary_key_bytes,
    })
}

fn collect_primary_key_values(
    row: &[Value],
    columns: &[ColumnDef],
    primary_idx: &IndexDef,
    table_name: &str,
) -> Result<Vec<Value>, DbError> {
    if !primary_idx.is_primary || primary_idx.columns.is_empty() {
        return Err(DbError::InvalidValue {
            reason: format!(
                "clustered row encoding for '{table_name}' requires a populated primary index"
            ),
        });
    }

    let mut values = Vec::with_capacity(primary_idx.columns.len());
    for key_col in &primary_idx.columns {
        let col_idx = key_col.col_idx as usize;
        let column = columns.get(col_idx).ok_or_else(|| DbError::InvalidValue {
            reason: format!(
                "clustered primary index '{}' references missing column {} on '{}'",
                primary_idx.name, key_col.col_idx, table_name
            ),
        })?;
        let value = row
            .get(col_idx)
            .cloned()
            .ok_or_else(|| DbError::InvalidValue {
                reason: format!(
                    "clustered row for '{table_name}' is missing primary-key column '{}'",
                    column.name
                ),
            })?;

        if matches!(value, Value::Null) {
            return Err(DbError::NotNullViolation {
                table: table_name.to_string(),
                column: column.name.clone(),
            });
        }

        values.push(value);
    }

    Ok(values)
}
