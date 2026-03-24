//! Index maintenance — keeps secondary indexes in sync with DML operations.
//!
//! Every INSERT, UPDATE, and DELETE must call the appropriate helper so that
//! all non-primary secondary indexes stay consistent with the heap.
//!
//! ## API
//!
//! - [`indexes_for_table`] — loads all `IndexDef`s for a table.
//! - [`insert_into_indexes`] — called after a successful heap INSERT.
//! - [`delete_from_indexes`] — called after a successful heap DELETE.
//!
//! ## Root-page persistence after splits
//!
//! When `BTree::insert_in` causes a root split, the root page ID changes.
//! `insert_into_indexes` returns a `Vec<(index_id, new_root_page_id)>` for any
//! indexes whose root changed.  The caller must persist these updates via
//! `CatalogWriter::update_index_root`.

use std::sync::atomic::{AtomicU64, Ordering};

use axiomdb_catalog::{CatalogReader, IndexDef};
use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot};
use axiomdb_index::BTree;
use axiomdb_storage::StorageEngine;
use axiomdb_types::Value;

use crate::key_encoding::encode_index_key;

// ── indexes_for_table ─────────────────────────────────────────────────────────

/// Returns all `IndexDef`s for the given table (including primary indexes).
///
/// The caller can filter with `!idx.is_primary` to get only secondary indexes.
pub fn indexes_for_table(
    table_id: u32,
    storage: &mut dyn StorageEngine,
    snapshot: TransactionSnapshot,
) -> Result<Vec<IndexDef>, DbError> {
    let mut reader = CatalogReader::new(storage, snapshot)?;
    reader.list_indexes(table_id)
}

// ── insert_into_indexes ───────────────────────────────────────────────────────

/// Inserts `(key → rid)` into every non-primary secondary index for the table.
///
/// For UNIQUE indexes, checks for duplicate keys before inserting (NULL values
/// skip the uniqueness check — NULL ≠ NULL in SQL).
///
/// Returns a list of `(index_id, new_root_page_id)` for indexes whose root
/// changed due to a B-Tree split.  The caller should persist these via
/// `CatalogWriter::update_index_root`.
pub fn insert_into_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    rid: RecordId,
    storage: &mut dyn StorageEngine,
) -> Result<Vec<(u32, u64)>, DbError> {
    let mut updated_roots = Vec::new();

    for idx in indexes
        .iter()
        .filter(|i| !i.is_primary && !i.columns.is_empty())
    {
        let key_vals: Vec<Value> = idx
            .columns
            .iter()
            .map(|c| row.get(c.col_idx as usize).cloned().unwrap_or(Value::Null))
            .collect();

        // Skip NULL key values — NULLs are not indexed in secondary indexes.
        // This is consistent with SQL semantics (NULL ≠ NULL) and avoids
        // DuplicateKey errors from the B-Tree when multiple NULLs are inserted
        // into a UNIQUE index.
        if key_vals.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }

        let key = encode_index_key(&key_vals)?;

        // Uniqueness check.
        if idx.is_unique && BTree::lookup_in(storage, idx.root_page_id, &key)?.is_some() {
            let col_name = idx
                .columns
                .first()
                .map(|c| format!("col_idx={}", c.col_idx))
                .unwrap_or_default();
            return Err(DbError::UniqueViolation {
                table: format!("index_id={}", idx.index_id),
                column: col_name,
            });
        }

        let root_pid = AtomicU64::new(idx.root_page_id);
        BTree::insert_in(storage, &root_pid, &key, rid)?;
        let new_root = root_pid.load(Ordering::Acquire);
        if new_root != idx.root_page_id {
            updated_roots.push((idx.index_id, new_root));
        }
    }

    Ok(updated_roots)
}

// ── delete_from_indexes ───────────────────────────────────────────────────────

/// Removes the entry for `rid` from every non-primary secondary index.
///
/// Encodes the key from `row` and calls `BTree::delete_in` on each index.
/// Not an error if the key is not found (e.g., index was created after the row).
///
/// Returns a list of `(index_id, new_root_page_id)` for indexes whose root
/// changed due to a collapse after deletion.
pub fn delete_from_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    storage: &mut dyn StorageEngine,
) -> Result<Vec<(u32, u64)>, DbError> {
    let mut updated_roots = Vec::new();

    for idx in indexes
        .iter()
        .filter(|i| !i.is_primary && !i.columns.is_empty())
    {
        let key_vals: Vec<Value> = idx
            .columns
            .iter()
            .map(|c| row.get(c.col_idx as usize).cloned().unwrap_or(Value::Null))
            .collect();

        // Skip NULL key values — NULLs were not inserted into the index.
        if key_vals.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }

        let key = match encode_index_key(&key_vals) {
            Ok(k) => k,
            Err(DbError::IndexKeyTooLong { .. }) => continue, // row was never indexed
            Err(e) => return Err(e),
        };

        let root_pid = AtomicU64::new(idx.root_page_id);
        // Ignore NotFound (key may not exist if index was created after the row).
        let _ = BTree::delete_in(storage, &root_pid, &key)?;
        let new_root = root_pid.load(Ordering::Acquire);
        if new_root != idx.root_page_id {
            updated_roots.push((idx.index_id, new_root));
        }
    }

    Ok(updated_roots)
}
