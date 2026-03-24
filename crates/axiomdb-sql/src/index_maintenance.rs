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

use crate::{eval::eval, eval::is_truthy, expr::Expr, key_encoding::encode_index_key};

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
/// For **partial indexes** (where `idx.predicate.is_some()`), `compiled_preds[i]`
/// holds the pre-compiled predicate expression for `indexes[i]`. If the predicate
/// is not satisfied by `row`, the index is skipped entirely (no B-Tree insert, no
/// uniqueness check). Callers produce `compiled_preds` via
/// [`crate::partial_index::compile_index_predicates`] once per statement.
///
/// Passing `&[]` for `compiled_preds` is equivalent to "no predicates" — all
/// indexes are treated as full indexes regardless of their stored predicate.
///
/// Returns a list of `(index_id, new_root_page_id)` for indexes whose root
/// changed due to a B-Tree split.  The caller should persist these via
/// `CatalogWriter::update_index_root`.
pub fn insert_into_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    rid: RecordId,
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
    compiled_preds: &[Option<Expr>],
) -> Result<Vec<(u32, u64)>, DbError> {
    let mut updated_roots = Vec::new();

    for (i, idx) in indexes
        .iter()
        .enumerate()
        .filter(|(_, i)| !i.is_primary && !i.columns.is_empty())
    {
        // Partial index predicate check (Phase 6.7).
        // compiled_preds[i] is None for full indexes OR when caller passes &[].
        if let Some(Some(pred)) = compiled_preds.get(i) {
            if !is_truthy(&eval(pred, row)?) {
                continue; // row doesn't satisfy predicate → skip this index
            }
        }

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
            // Use the index name as the key identifier (most readable without a
            // catalog read) and the duplicate value as the column field.
            // Message: "unique key violation on uq_email.alice@x.com"
            let dup_val = key_vals.first().map(|v| format!("{v}")).unwrap_or_default();
            return Err(DbError::UniqueViolation {
                table: idx.name.clone(),
                column: dup_val,
            });
        }

        let root_pid = AtomicU64::new(idx.root_page_id);
        BTree::insert_in(storage, &root_pid, &key, rid)?;
        bloom.add(idx.index_id, &key);
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
/// For **partial indexes**, if the row does not satisfy the predicate the row was
/// never indexed — the delete is skipped. Pass compiled predicates via
/// `compiled_preds` (parallel to `indexes`); pass `&[]` to treat all as full indexes.
///
/// Returns a list of `(index_id, new_root_page_id)` for indexes whose root
/// changed due to a collapse after deletion.
pub fn delete_from_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
    compiled_preds: &[Option<Expr>],
) -> Result<Vec<(u32, u64)>, DbError> {
    let mut updated_roots = Vec::new();

    for (i, idx) in indexes
        .iter()
        .enumerate()
        .filter(|(_, i)| !i.is_primary && !i.columns.is_empty())
    {
        // Partial index predicate check (Phase 6.7).
        if let Some(Some(pred)) = compiled_preds.get(i) {
            if !is_truthy(&eval(pred, row)?) {
                continue; // row was never in this index → nothing to delete
            }
        }
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
        bloom.mark_dirty(idx.index_id);
        let new_root = root_pid.load(Ordering::Acquire);
        if new_root != idx.root_page_id {
            updated_roots.push((idx.index_id, new_root));
        }
    }

    Ok(updated_roots)
}
