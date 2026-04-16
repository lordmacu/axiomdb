//! Foreign key constraint enforcement — INSERT/UPDATE/DELETE validation (Phase 6.5/6.6).
//!
//! ## Design
//!
//! Enforcement is split by operation:
//!
//! - **INSERT / UPDATE child** — checks that the new FK value references an
//!   existing parent row. Uses the parent's PK/UNIQUE index for O(log n)
//!   lookup with a Bloom filter shortcut.
//!
//! - **DELETE parent** — before any parent row is physically deleted, checks
//!   all FK constraints that reference this parent table:
//!   - RESTRICT / NO ACTION → error if children exist
//!   - CASCADE → delete children recursively (depth-limited to 10)
//!   - SET NULL → update children's FK column to NULL
//!
//! ## NULL semantics
//!
//! NULL FK values are exempt from all checks (SQL standard MATCH SIMPLE).
//!
//! ## Non-unique index limitation (Phase 6.5)
//!
//! The current B-Tree implementation stores at most one `RecordId` per key in
//! non-unique indexes. For RESTRICT checks this is fine — one match is enough
//! to know children exist. For CASCADE / SET NULL, a full table scan is used
//! to guarantee ALL matching children are found.

use std::sync::atomic::{AtomicU64, Ordering};

/// `(pk_key_bytes, decoded_row)` pair returned by clustered-child scan helpers.
type PkRowPair = (Vec<u8>, Vec<Value>);

use axiomdb_catalog::{schema::FkAction, schema::IndexDef, CatalogReader, CatalogWriter, FkDef};
use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot};
use axiomdb_index::BTree;
use axiomdb_storage::{heap::RowHeader, heap_chain::HeapChain, StorageEngine};
use axiomdb_types::{codec::encode_row, Value};
use axiomdb_wal::{ClusteredRowImage, TxnManager};

use crate::{
    bloom::BloomRegistry,
    clustered_secondary::ClusteredSecondaryLayout,
    key_encoding::encode_index_key,
    table::{column_data_types, TableEngine},
};

/// Maximum ON DELETE CASCADE recursion depth.
/// Matches InnoDB's `FK_MAX_CASCADE_DEL`. Prevents infinite loops in circular graphs.
const MAX_CASCADE_DEPTH: u32 = 10;

/// Computes the replacement value for an FK child column when the referential
/// action is `SET NULL` or `SET DEFAULT` (GAP-C.4).
///
/// For `SET DEFAULT`, evaluates the stored default expression of the child
/// column. Returns `Value::Null` when no default is declared (matches the
/// PostgreSQL fallback — a child with no default behaves like `SET NULL`).
fn fk_replacement_value(
    action: FkAction,
    child_cols: &[axiomdb_catalog::schema::ColumnDef],
    child_col_idx: u16,
) -> Value {
    if action != FkAction::SetDefault {
        return Value::Null;
    }
    let col = match child_cols.iter().find(|c| c.col_idx == child_col_idx) {
        Some(c) => c,
        None => return Value::Null,
    };
    let expr_str = match &col.default_expr {
        Some(s) => s,
        None => return Value::Null,
    };
    match crate::parser::parse_expr_only(expr_str) {
        Ok(expr) => crate::eval::eval(&expr, &[]).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

// ── INSERT / UPDATE child ─────────────────────────────────────────────────────

/// Validates that `row` satisfies all FK constraints in `foreign_keys`.
///
/// For each FK:
/// 1. NULL FK value → skip (MATCH SIMPLE exemption).
/// 2. Encode the FK value as an index key.
/// 3. Bloom shortcut on the parent's PK/UNIQUE index.
/// 4. B-Tree point lookup on the parent index.
/// 5. No match → `ForeignKeyViolation`.
pub fn check_fk_child_insert(
    row: &[Value],
    foreign_keys: &[FkDef],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    bloom: &BloomRegistry,
) -> Result<(), DbError> {
    if foreign_keys.is_empty() {
        return Ok(());
    }
    // Bloom is now used for FK parent lookup (Phase 6.9: PK B-Trees populated).

    let snap = txn.active_snapshot(conn_txn);

    for fk in foreign_keys {
        // Collect values for every child FK column (composite FKs: GAP-C.2).
        let fk_vals: Vec<&Value> = fk
            .child_col_idxs
            .iter()
            .map(|idx| row.get(*idx as usize).unwrap_or(&Value::Null))
            .collect();

        // NULL on any FK column → MATCH SIMPLE passes.
        if fk_vals.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }

        let owned_vals: Vec<Value> = fk_vals.iter().map(|v| (*v).clone()).collect();
        let key = encode_index_key(&owned_vals)?;

        // Find a parent PRIMARY KEY or UNIQUE index whose leading columns
        // match `fk.parent_col_idxs` in order.
        let (parent_index_id, parent_index_root, parent_clustered_primary) = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            let parent_def = reader.get_table_by_id(fk.parent_table_id)?.ok_or(
                DbError::CatalogTableNotFound {
                    table_id: fk.parent_table_id,
                },
            )?;
            let parent_indexes = reader.list_indexes(fk.parent_table_id)?;
            let parent_idx = parent_indexes
                .iter()
                .find(|i| {
                    (i.is_primary || i.is_unique)
                        && i.columns.len() >= fk.parent_col_idxs.len()
                        && i.columns
                            .iter()
                            .take(fk.parent_col_idxs.len())
                            .zip(fk.parent_col_idxs.iter())
                            .all(|(ic, wanted)| ic.col_idx == *wanted)
                })
                .ok_or_else(|| {
                    let (tname, cname) =
                        resolve_names(storage, snap.clone(), fk.parent_table_id, fk.parent_col_idx);
                    DbError::ForeignKeyNoParentIndex {
                        table: tname,
                        column: cname,
                    }
                })?;
            (
                parent_idx.index_id,
                parent_idx.root_page_id,
                parent_def.is_clustered() && parent_idx.is_primary,
            )
        }; // reader dropped here

        // Phase 6.9: PK B-Trees are now populated via insert_into_indexes
        // (the `!is_primary` filter was removed). All index types use B-Tree lookup.
        //
        // Bloom shortcut: if the filter says definitely absent, skip B-Tree entirely.
        // For composite keys we use the first column's bloom as a heuristic — a
        // false-positive here is fine because the B-Tree lookup below is exact.
        let bloom_lookup_key = if fk.child_col_idxs.len() == 1 {
            key.clone()
        } else {
            encode_index_key(std::slice::from_ref(&owned_vals[0]))?
        };
        if !bloom.might_exist(parent_index_id, &bloom_lookup_key) && fk.child_col_idxs.len() == 1 {
            let (tname, cname) = resolve_names(storage, snap, fk.child_table_id, fk.child_col_idx);
            return Err(DbError::ForeignKeyViolation {
                table: tname,
                column: cname,
                value: format!("{}", fk_vals[0]),
            });
        }

        // For composite FKs whose parent index has MORE columns than the FK
        // references (e.g. FK on (a,b) against PK (a,b,c)), we need a range
        // scan over the key prefix rather than a point lookup. Otherwise an
        // exact-match lookup would never succeed.
        let parent_idx_col_count = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader
                .list_indexes(fk.parent_table_id)?
                .into_iter()
                .find(|i| i.index_id == parent_index_id)
                .map(|i| i.columns.len())
                .unwrap_or(fk.parent_col_idxs.len())
        };
        let parent_exists = if parent_idx_col_count == fk.parent_col_idxs.len() {
            if parent_clustered_primary {
                axiomdb_storage::clustered_tree::lookup(
                    storage,
                    Some(parent_index_root),
                    &key,
                    &snap,
                )?
                .is_some()
            } else {
                BTree::lookup_in(storage, parent_index_root, &key)?.is_some()
            }
        } else {
            // Prefix scan: match any parent index entry whose leading bytes
            // equal `key`. `key` already encodes exactly the FK columns.
            let mut upper = key.clone();
            upper.push(0xff);
            !BTree::range_in(storage, parent_index_root, Some(&key), Some(&upper))?.is_empty()
        };

        if !parent_exists {
            let (tname, cname) = resolve_names(storage, snap, fk.child_table_id, fk.child_col_idx);
            return Err(DbError::ForeignKeyViolation {
                table: tname,
                column: cname,
                value: format!("{}", fk_vals[0]),
            });
        }
    }

    Ok(())
}

/// Validates FK constraints for UPDATE on a child table.
///
/// Only checks FK columns whose value changed between `old_row` and `new_row`.
pub fn check_fk_child_update(
    old_row: &[Value],
    new_row: &[Value],
    foreign_keys: &[FkDef],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &axiomdb_wal::ConnectionTxn,
    bloom: &BloomRegistry,
) -> Result<(), DbError> {
    if foreign_keys.is_empty() {
        return Ok(());
    }

    let changed_fks: Vec<FkDef> = foreign_keys
        .iter()
        .filter(|fk| {
            let old_val = old_row
                .get(fk.child_col_idx as usize)
                .unwrap_or(&Value::Null);
            let new_val = new_row
                .get(fk.child_col_idx as usize)
                .unwrap_or(&Value::Null);
            old_val != new_val
        })
        .cloned()
        .collect();

    if changed_fks.is_empty() {
        return Ok(());
    }

    check_fk_child_insert(new_row, &changed_fks, storage, txn, conn_txn, bloom)
}

// ── DELETE parent ─────────────────────────────────────────────────────────────

/// Enforces FK constraints when rows are deleted from `parent_table_id`.
///
/// Must be called **before** deleting the parent rows from the heap so that:
/// - RESTRICT can abort cleanly (parent rows still exist).
/// - CASCADE can read child rows before they become orphaned.
///
/// `depth` tracks CASCADE recursion — pass `0` from the top-level DELETE.
pub fn enforce_fk_on_parent_delete(
    deleted_rows: &[(RecordId, Vec<Value>)],
    parent_table_id: u32,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    bloom: &BloomRegistry,
    depth: u32,
) -> Result<(), DbError> {
    if deleted_rows.is_empty() {
        return Ok(());
    }
    if depth > MAX_CASCADE_DEPTH {
        return Err(DbError::ForeignKeyCascadeDepth {
            limit: MAX_CASCADE_DEPTH,
        });
    }

    let snap = txn.active_snapshot(conn_txn);

    // Load all FK constraints referencing this table as parent.
    let fk_list = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader.list_fk_constraints_referencing(parent_table_id)?
    };
    if fk_list.is_empty() {
        return Ok(());
    }

    for fk in &fk_list {
        // Load child table metadata.
        let child_table_def = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader
                .get_table_by_id(fk.child_table_id)?
                .ok_or(DbError::CatalogTableNotFound {
                    table_id: fk.child_table_id,
                })?
        };
        let child_cols = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader.list_columns(fk.child_table_id)?
        };

        let child_col_name = child_cols
            .iter()
            .find(|c| c.col_idx == fk.child_col_idx)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("col_{}", fk.child_col_idx));

        // Pre-validate SET NULL compatibility before touching any data.
        if matches!(fk.on_delete, FkAction::SetNull) {
            let nullable = child_cols
                .iter()
                .find(|c| c.col_idx == fk.child_col_idx)
                .map(|c| c.nullable)
                .unwrap_or(true);
            if !nullable {
                return Err(DbError::ForeignKeySetNullNotNullable {
                    table: child_table_def.table_name.clone(),
                    column: child_col_name.clone(),
                });
            }
        }

        // Find the FK auto-index on the child (Phase 6.9: composite key index).
        // fk_index_id != 0 means a composite-key FK auto-index was created.
        // fk_index_id == 0 means the user provided their own index (or pre-6.9 FK).
        let fk_index_root: Option<u64> = if fk.fk_index_id != 0 {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader
                .list_indexes(fk.child_table_id)?
                .into_iter()
                .find(|i| i.index_id == fk.fk_index_id)
                .map(|i| i.root_page_id)
        } else {
            None // pre-6.9 FK or user-provided index — use full scan
        };

        for (_, parent_row) in deleted_rows {
            let parent_key_val = parent_row
                .get(fk.parent_col_idx as usize)
                .unwrap_or(&Value::Null);

            // Parent key is NULL → no child can reference NULL.
            if matches!(parent_key_val, Value::Null) {
                continue;
            }

            match fk.on_delete {
                FkAction::NoAction | FkAction::Restrict => {
                    // Phase 6.9: use FK composite index for O(log n) existence check.
                    let has_child = if let Some(root) = fk_index_root {
                        let (lo, hi) = crate::index_maintenance::fk_key_range(parent_key_val)?;
                        let entries = BTree::range_in(storage, root, Some(&lo), Some(&hi))?;
                        // Check heap visibility: dead FK index entries (from deferred
                        // deletion) must not cause false RESTRICT violations.
                        entries.iter().any(|(rid, _)| {
                            HeapChain::is_slot_visible(
                                storage,
                                rid.page_id,
                                rid.slot_id,
                                snap.clone(),
                            )
                            .unwrap_or(false)
                        })
                    } else {
                        // Pre-6.9 FK or user index: fall back to full scan.
                        children_exist_via_scan(
                            storage,
                            &child_table_def,
                            &child_cols,
                            fk.child_col_idx,
                            parent_key_val,
                            snap.clone(),
                        )?
                    };

                    if has_child {
                        return Err(DbError::ForeignKeyParentViolation {
                            constraint: fk.name.clone(),
                            child_table: child_table_def.table_name.clone(),
                            child_column: child_col_name.clone(),
                        });
                    }
                }

                FkAction::Cascade => {
                    if child_table_def.is_clustered() {
                        // Clustered child: scan with PK keys, apply delete-marks.
                        // fk_index_root is always None for clustered children.
                        let children_with_pk = find_clustered_children_with_pk(
                            storage,
                            &child_table_def,
                            &child_cols,
                            fk.child_col_idx,
                            parent_key_val,
                            snap.clone(),
                        )?;

                        if children_with_pk.is_empty() {
                            continue;
                        }

                        // Recursive FK enforcement (dummy RIDs — only values matter).
                        let child_rows_for_recursive: Vec<(RecordId, Vec<Value>)> =
                            children_with_pk
                                .iter()
                                .map(|(_, vals)| {
                                    (
                                        RecordId {
                                            page_id: 0,
                                            slot_id: 0,
                                        },
                                        vals.clone(),
                                    )
                                })
                                .collect();
                        enforce_fk_on_parent_delete(
                            &child_rows_for_recursive,
                            fk.child_table_id,
                            storage,
                            txn,
                            conn_txn,
                            bloom,
                            depth + 1,
                        )?;

                        // Delete-mark all matching clustered rows.
                        // Root may have been updated by recursive operations above.
                        let current_root = txn
                            .clustered_root(fk.child_table_id)
                            .unwrap_or(child_table_def.root_page_id);
                        let pk_keys: Vec<Vec<u8>> =
                            children_with_pk.iter().map(|(k, _)| k.clone()).collect();
                        delete_mark_clustered_rows(
                            storage,
                            txn,
                            conn_txn,
                            fk.child_table_id,
                            current_root,
                            &pk_keys,
                            snap.clone(),
                        )?;
                        // Secondary index entries left in place — MVCC deferred cleanup
                        // (same behavior as regular clustered DELETE executor).
                    } else {
                        // Heap child: FK index range scan if available, else full scan.
                        let child_rows = if let Some(root) = fk_index_root {
                            let (lo, hi) = crate::index_maintenance::fk_key_range(parent_key_val)?;
                            let entries = BTree::range_in(storage, root, Some(&lo), Some(&hi))?;
                            let mut rows = Vec::with_capacity(entries.len());
                            for (child_rid, _) in entries {
                                if !HeapChain::is_slot_visible(
                                    storage,
                                    child_rid.page_id,
                                    child_rid.slot_id,
                                    snap.clone(),
                                )? {
                                    continue;
                                }
                                let row_bytes = axiomdb_storage::heap_chain::HeapChain::read_row(
                                    storage,
                                    child_rid.page_id,
                                    child_rid.slot_id,
                                )?;
                                if let Some(bytes) = row_bytes {
                                    let vals =
                                        crate::table::decode_row_from_bytes(&bytes, &child_cols)?;
                                    rows.push((child_rid, vals));
                                }
                            }
                            rows
                        } else {
                            find_children_via_scan(
                                storage,
                                &child_table_def,
                                &child_cols,
                                fk.child_col_idx,
                                parent_key_val,
                                snap.clone(),
                            )?
                        };

                        if child_rows.is_empty() {
                            continue;
                        }

                        // Recursively enforce FK on children's children BEFORE deleting.
                        enforce_fk_on_parent_delete(
                            &child_rows,
                            fk.child_table_id,
                            storage,
                            txn,
                            conn_txn,
                            bloom,
                            depth + 1,
                        )?;

                        // Batch-delete children from the heap.
                        let child_rids: Vec<RecordId> =
                            child_rows.iter().map(|(rid, _)| *rid).collect();
                        crate::table::TableEngine::delete_rows_batch(
                            storage,
                            txn,
                            conn_txn,
                            &child_table_def,
                            &child_rids,
                        )?;

                        // Maintain secondary indexes on the child table.
                        let mut current_secondary = {
                            let mut reader = CatalogReader::new(storage, snap.clone())?;
                            let all = reader.list_indexes(fk.child_table_id)?;
                            all.into_iter()
                                .filter(|i| !i.columns.is_empty())
                                .collect::<Vec<_>>()
                        };
                        if !current_secondary.is_empty() {
                            for (child_rid, child_row_vals) in &child_rows {
                                let updated = crate::index_maintenance::delete_from_indexes(
                                    &current_secondary,
                                    child_row_vals,
                                    *child_rid,
                                    storage,
                                    bloom,
                                    &[],
                                    &[],
                                )?;
                                for (index_id, new_root) in updated {
                                    CatalogWriter::new(storage, txn, conn_txn)?
                                        .update_index_root(index_id, new_root)?;
                                    if let Some(idx) = current_secondary
                                        .iter_mut()
                                        .find(|i| i.index_id == index_id)
                                    {
                                        idx.root_page_id = new_root;
                                    }
                                }
                            }
                        }
                    }
                }

                FkAction::SetNull | FkAction::SetDefault => {
                    let replacement =
                        fk_replacement_value(fk.on_delete, &child_cols, fk.child_col_idx);
                    if child_table_def.is_clustered() {
                        // Clustered child: scan with PK keys, then delete-mark + re-insert
                        // each row with the FK column set to NULL.
                        let children_with_pk = find_clustered_children_with_pk(
                            storage,
                            &child_table_def,
                            &child_cols,
                            fk.child_col_idx,
                            parent_key_val,
                            snap.clone(),
                        )?;

                        if children_with_pk.is_empty() {
                            continue;
                        }

                        let primary_idx = {
                            let mut reader = CatalogReader::new(storage, snap.clone())?;
                            reader
                                .list_indexes(fk.child_table_id)?
                                .into_iter()
                                .find(|i| i.is_primary && !i.columns.is_empty())
                                .ok_or_else(|| DbError::Internal {
                                    message: format!(
                                        "clustered child table {} missing primary index",
                                        fk.child_table_id
                                    ),
                                })?
                        };
                        let mut secondary_idxs: Vec<IndexDef> = {
                            let mut reader = CatalogReader::new(storage, snap.clone())?;
                            reader
                                .list_indexes(fk.child_table_id)?
                                .into_iter()
                                .filter(|i| !i.is_primary && !i.columns.is_empty())
                                .collect()
                        };

                        let mut current_root = txn
                            .clustered_root(fk.child_table_id)
                            .unwrap_or(child_table_def.root_page_id);

                        for (pk_key, old_values) in &children_with_pk {
                            let mut new_values = old_values.clone();
                            new_values[fk.child_col_idx as usize] = replacement.clone();
                            current_root = apply_clustered_set_null(
                                storage,
                                txn,
                                conn_txn,
                                fk.child_table_id,
                                current_root,
                                &primary_idx,
                                &mut secondary_idxs,
                                &child_cols,
                                pk_key,
                                old_values,
                                &new_values,
                                snap.clone(),
                            )?;
                        }
                    } else {
                        // Heap child: FK index range scan if available, else full scan.
                        let child_rows = if let Some(root) = fk_index_root {
                            let (lo, hi) = crate::index_maintenance::fk_key_range(parent_key_val)?;
                            let entries = BTree::range_in(storage, root, Some(&lo), Some(&hi))?;
                            let mut rows = Vec::with_capacity(entries.len());
                            for (child_rid, _) in entries {
                                if !HeapChain::is_slot_visible(
                                    storage,
                                    child_rid.page_id,
                                    child_rid.slot_id,
                                    snap.clone(),
                                )? {
                                    continue;
                                }
                                let row_bytes = axiomdb_storage::heap_chain::HeapChain::read_row(
                                    storage,
                                    child_rid.page_id,
                                    child_rid.slot_id,
                                )?;
                                if let Some(bytes) = row_bytes {
                                    let vals =
                                        crate::table::decode_row_from_bytes(&bytes, &child_cols)?;
                                    rows.push((child_rid, vals));
                                }
                            }
                            rows
                        } else {
                            find_children_via_scan(
                                storage,
                                &child_table_def,
                                &child_cols,
                                fk.child_col_idx,
                                parent_key_val,
                                snap.clone(),
                            )?
                        };

                        if child_rows.is_empty() {
                            continue;
                        }

                        let mut current_indexes = {
                            let mut reader = CatalogReader::new(storage, snap.clone())?;
                            let all = reader.list_indexes(fk.child_table_id)?;
                            all.into_iter()
                                .filter(|i| !i.columns.is_empty())
                                .collect::<Vec<_>>()
                        };
                        let compiled_preds = crate::partial_index::compile_index_predicates(
                            &current_indexes,
                            &child_cols,
                        )?;
                        let mut update_pairs: Vec<(RecordId, Vec<Value>, RecordId, Vec<Value>)> =
                            Vec::with_capacity(child_rows.len());

                        for (child_rid, child_row) in &child_rows {
                            let mut new_child_row = child_row.clone();
                            new_child_row[fk.child_col_idx as usize] = replacement.clone();

                            let new_rid = TableEngine::update_row(
                                storage,
                                txn,
                                conn_txn,
                                &child_table_def,
                                &child_cols,
                                *child_rid,
                                new_child_row.clone(),
                            )?;
                            update_pairs.push((
                                *child_rid,
                                child_row.clone(),
                                new_rid,
                                new_child_row,
                            ));
                        }

                        for (idx_pos, idx) in current_indexes.iter_mut().enumerate() {
                            if idx.columns.is_empty() {
                                continue;
                            }

                            let pred = compiled_preds.get(idx_pos).and_then(|p| p.as_ref());
                            let mut delete_keys: Vec<Vec<u8>> = Vec::new();
                            let mut insert_rows: Vec<(RecordId, &Vec<Value>)> = Vec::new();

                            for (old_rid, old_values, new_rid, new_values) in &update_pairs {
                                if crate::index_maintenance::update_affects_index(
                                    idx, pred, old_values, *old_rid, new_values, *new_rid, None,
                                )? {
                                    if let Some(key_vals) =
                                        crate::index_maintenance::index_key_values_if_indexed(
                                            idx, old_values, pred,
                                        )?
                                    {
                                        delete_keys.push(
                                            crate::index_maintenance::encode_index_entry_key(
                                                idx, &key_vals, *old_rid,
                                            )?,
                                        );
                                    }
                                    insert_rows.push((*new_rid, new_values));
                                }
                            }

                            if !delete_keys.is_empty() {
                                delete_keys.sort_unstable();
                                if let Some(new_root) =
                                    crate::index_maintenance::delete_many_from_single_index(
                                        idx,
                                        &delete_keys,
                                        storage,
                                        bloom,
                                    )?
                                {
                                    CatalogWriter::new(storage, txn, conn_txn)?
                                        .update_index_root(idx.index_id, new_root)?;
                                }
                            }

                            if !insert_rows.is_empty() {
                                let batch_refs: Vec<(&[Value], RecordId)> = insert_rows
                                    .iter()
                                    .map(|(rid, vals)| (vals.as_slice(), *rid))
                                    .collect();
                                if let Some(new_root) =
                                    crate::index_maintenance::insert_many_into_single_index(
                                        idx,
                                        pred,
                                        &batch_refs,
                                        storage,
                                        bloom,
                                        snap.clone(),
                                    )?
                                {
                                    CatalogWriter::new(storage, txn, conn_txn)?
                                        .update_index_root(idx.index_id, new_root)?;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

/// Enforces FK constraints when the referenced parent key columns are updated.
///
/// Only RESTRICT / NO ACTION are supported. CASCADE / SET NULL on UPDATE are
pub fn enforce_fk_on_parent_update(
    old_rows: &[(RecordId, Vec<Value>)],
    new_values_per_row: &[Vec<Value>],
    parent_table_id: u32,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    bloom: &crate::BloomRegistry,
) -> Result<(), DbError> {
    if old_rows.is_empty() {
        return Ok(());
    }

    let snap = txn.active_snapshot(conn_txn);
    let fk_list = {
        let mut reader = CatalogReader::new(storage, snap.clone())?;
        reader.list_fk_constraints_referencing(parent_table_id)?
    };
    if fk_list.is_empty() {
        return Ok(());
    }

    for fk in &fk_list {
        let child_table_def = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader
                .get_table_by_id(fk.child_table_id)?
                .ok_or(DbError::CatalogTableNotFound {
                    table_id: fk.child_table_id,
                })?
        };
        let child_cols = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader.list_columns(fk.child_table_id)?
        };
        let child_col_name = child_cols
            .iter()
            .find(|c| c.col_idx == fk.child_col_idx)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("col_{}", fk.child_col_idx));

        // Use FK composite index if available (fk_index_id != 0), else fallback to scan.
        let fk_index_root: Option<u64> = if fk.fk_index_id != 0 {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader
                .list_indexes(fk.child_table_id)?
                .into_iter()
                .find(|i| i.index_id == fk.fk_index_id)
                .map(|i| i.root_page_id)
        } else {
            None
        };

        for ((_, old_values), new_values) in old_rows.iter().zip(new_values_per_row.iter()) {
            let old_key_val = old_values
                .get(fk.parent_col_idx as usize)
                .unwrap_or(&Value::Null);
            let new_key_val = new_values
                .get(fk.parent_col_idx as usize)
                .unwrap_or(&Value::Null);

            // Referenced column unchanged → no FK check needed.
            if old_key_val == new_key_val || matches!(old_key_val, Value::Null) {
                continue;
            }

            // Phase 6.9: use FK composite index range scan if available.
            let has_children = if let Some(root) = fk_index_root {
                let (lo, hi) = crate::index_maintenance::fk_key_range(old_key_val)?;
                !BTree::range_in(storage, root, Some(&lo), Some(&hi))?.is_empty()
            } else {
                children_exist_via_scan(
                    storage,
                    &child_table_def,
                    &child_cols,
                    fk.child_col_idx,
                    old_key_val,
                    snap.clone(),
                )?
            };

            if has_children {
                match fk.on_update {
                    FkAction::NoAction | FkAction::Restrict => {
                        return Err(DbError::ForeignKeyParentViolation {
                            constraint: fk.name.clone(),
                            child_table: child_table_def.table_name.clone(),
                            child_column: child_col_name.clone(),
                        });
                    }
                    FkAction::Cascade | FkAction::SetNull | FkAction::SetDefault => {
                        let replacement = match fk.on_update {
                            FkAction::Cascade => new_key_val.clone(),
                            FkAction::SetNull => Value::Null,
                            FkAction::SetDefault => fk_replacement_value(
                                FkAction::SetDefault,
                                &child_cols,
                                fk.child_col_idx,
                            ),
                            _ => unreachable!(),
                        };
                        apply_fk_update_children(
                            storage,
                            txn,
                            conn_txn,
                            bloom,
                            &child_table_def,
                            &child_cols,
                            fk,
                            fk_index_root,
                            old_key_val,
                            &replacement,
                            snap.clone(),
                        )?;
                    }
                }
            }
        }
    }

    Ok(())
}

/// Updates all child rows whose FK column equals `old_parent_val`, setting the
/// FK column to `replacement_val`.  Handles both clustered and heap child tables,
/// including secondary index maintenance.
///
/// Used by ON UPDATE CASCADE (`replacement_val` = new parent key) and
/// ON UPDATE SET NULL (`replacement_val` = `Value::Null`).
#[allow(clippy::too_many_arguments)]
fn apply_fk_update_children(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    bloom: &crate::BloomRegistry,
    child_table_def: &axiomdb_catalog::schema::TableDef,
    child_cols: &[axiomdb_catalog::schema::ColumnDef],
    fk: &FkDef,
    fk_index_root: Option<u64>,
    old_parent_val: &Value,
    replacement_val: &Value,
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<(), DbError> {
    if child_table_def.is_clustered() {
        // ── Clustered child table ────────────────────────────────────
        let children_with_pk = find_clustered_children_with_pk(
            storage,
            child_table_def,
            child_cols,
            fk.child_col_idx,
            old_parent_val,
            snap.clone(),
        )?;
        if children_with_pk.is_empty() {
            return Ok(());
        }

        let primary_idx = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader
                .list_indexes(fk.child_table_id)?
                .into_iter()
                .find(|i| i.is_primary && !i.columns.is_empty())
                .ok_or_else(|| DbError::Internal {
                    message: format!(
                        "clustered child table {} missing primary index",
                        fk.child_table_id
                    ),
                })?
        };
        let mut secondary_idxs: Vec<IndexDef> = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            reader
                .list_indexes(fk.child_table_id)?
                .into_iter()
                .filter(|i| !i.is_primary && !i.columns.is_empty())
                .collect()
        };

        let mut current_root = txn
            .clustered_root(fk.child_table_id)
            .unwrap_or(child_table_def.root_page_id);

        for (pk_key, old_values) in &children_with_pk {
            let mut new_values = old_values.clone();
            new_values[fk.child_col_idx as usize] = replacement_val.clone();
            current_root = apply_clustered_set_null(
                storage,
                txn,
                conn_txn,
                fk.child_table_id,
                current_root,
                &primary_idx,
                &mut secondary_idxs,
                child_cols,
                pk_key,
                old_values,
                &new_values,
                snap.clone(),
            )?;
        }
    } else {
        // ── Heap child table ─────────────────────────────────────────
        let child_rows = if let Some(root) = fk_index_root {
            let (lo, hi) = crate::index_maintenance::fk_key_range(old_parent_val)?;
            let entries = BTree::range_in(storage, root, Some(&lo), Some(&hi))?;
            let mut rows = Vec::with_capacity(entries.len());
            for (child_rid, _) in entries {
                if !HeapChain::is_slot_visible(
                    storage,
                    child_rid.page_id,
                    child_rid.slot_id,
                    snap.clone(),
                )? {
                    continue;
                }
                let row_bytes = axiomdb_storage::heap_chain::HeapChain::read_row(
                    storage,
                    child_rid.page_id,
                    child_rid.slot_id,
                )?;
                if let Some(bytes) = row_bytes {
                    let vals = crate::table::decode_row_from_bytes(&bytes, child_cols)?;
                    rows.push((child_rid, vals));
                }
            }
            rows
        } else {
            find_children_via_scan(
                storage,
                child_table_def,
                child_cols,
                fk.child_col_idx,
                old_parent_val,
                snap.clone(),
            )?
        };

        if child_rows.is_empty() {
            return Ok(());
        }

        let mut current_indexes = {
            let mut reader = CatalogReader::new(storage, snap.clone())?;
            let all = reader.list_indexes(fk.child_table_id)?;
            all.into_iter()
                .filter(|i| !i.columns.is_empty())
                .collect::<Vec<_>>()
        };
        let compiled_preds =
            crate::partial_index::compile_index_predicates(&current_indexes, child_cols)?;
        let mut update_pairs: Vec<(RecordId, Vec<Value>, RecordId, Vec<Value>)> =
            Vec::with_capacity(child_rows.len());

        for (child_rid, child_row) in &child_rows {
            let mut new_child_row = child_row.clone();
            new_child_row[fk.child_col_idx as usize] = replacement_val.clone();

            let new_rid = TableEngine::update_row(
                storage,
                txn,
                conn_txn,
                child_table_def,
                child_cols,
                *child_rid,
                new_child_row.clone(),
            )?;
            update_pairs.push((*child_rid, child_row.clone(), new_rid, new_child_row));
        }

        // Maintain secondary indexes on the child table.
        for (idx_pos, idx) in current_indexes.iter_mut().enumerate() {
            if idx.columns.is_empty() {
                continue;
            }

            let pred = compiled_preds.get(idx_pos).and_then(|p| p.as_ref());
            let mut delete_keys: Vec<Vec<u8>> = Vec::new();
            let mut insert_rows: Vec<(RecordId, &Vec<Value>)> = Vec::new();

            for (old_rid, old_values, new_rid, new_values) in &update_pairs {
                if crate::index_maintenance::update_affects_index(
                    idx, pred, old_values, *old_rid, new_values, *new_rid, None,
                )? {
                    if let Some(key_vals) = crate::index_maintenance::index_key_values_if_indexed(
                        idx, old_values, pred,
                    )? {
                        delete_keys.push(crate::index_maintenance::encode_index_entry_key(
                            idx, &key_vals, *old_rid,
                        )?);
                    }
                    insert_rows.push((*new_rid, new_values));
                }
            }

            if !delete_keys.is_empty() {
                delete_keys.sort_unstable();
                if let Some(new_root) = crate::index_maintenance::delete_many_from_single_index(
                    idx,
                    &delete_keys,
                    storage,
                    bloom,
                )? {
                    CatalogWriter::new(storage, txn, conn_txn)?
                        .update_index_root(idx.index_id, new_root)?;
                }
            }

            if !insert_rows.is_empty() {
                let batch_refs: Vec<(&[Value], RecordId)> = insert_rows
                    .iter()
                    .map(|(rid, vals)| (vals.as_slice(), *rid))
                    .collect();
                if let Some(new_root) = crate::index_maintenance::insert_many_into_single_index(
                    idx,
                    pred,
                    &batch_refs,
                    storage,
                    bloom,
                    snap.clone(),
                )? {
                    CatalogWriter::new(storage, txn, conn_txn)?
                        .update_index_root(idx.index_id, new_root)?;
                }
            }
        }
    }
    Ok(())
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Returns `true` if any child row has FK column equal to `fk_val` (full scan).
fn children_exist_via_scan(
    storage: &dyn StorageEngine,
    child_table_def: &axiomdb_catalog::schema::TableDef,
    child_cols: &[axiomdb_catalog::schema::ColumnDef],
    child_col_idx: u16,
    fk_val: &Value,
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<bool, DbError> {
    let rows = if child_table_def.is_clustered() {
        crate::table::scan_clustered_table(storage, child_table_def, child_cols, snap)?
    } else {
        TableEngine::scan_table(storage, child_table_def, child_cols, snap, None)?
    };
    Ok(rows.iter().any(|(_, row)| {
        row.get(child_col_idx as usize)
            .map(|v| v == fk_val)
            .unwrap_or(false)
    }))
}

/// Returns all child rows where FK column equals `fk_val` (full scan).
///
/// Used for CASCADE and SET NULL where ALL matching children must be found.
/// Full scan is required because the FK index only stores ONE RecordId per key
/// value (B-Tree limitation in Phase 6.5 — multiple rows with the same FK value
/// are not all reachable via the index).
fn find_children_via_scan(
    storage: &dyn StorageEngine,
    child_table_def: &axiomdb_catalog::schema::TableDef,
    child_cols: &[axiomdb_catalog::schema::ColumnDef],
    child_col_idx: u16,
    fk_val: &Value,
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    let rows = if child_table_def.is_clustered() {
        crate::table::scan_clustered_table(storage, child_table_def, child_cols, snap)?
    } else {
        TableEngine::scan_table(storage, child_table_def, child_cols, snap, None)?
    };
    Ok(rows
        .into_iter()
        .filter(|(_, row)| {
            row.get(child_col_idx as usize)
                .map(|v| v == fk_val)
                .unwrap_or(false)
        })
        .collect())
}

/// Scans a clustered child table and returns `(pk_key_bytes, decoded_row)` for
/// every visible row where the FK column equals `fk_val`.
fn find_clustered_children_with_pk(
    storage: &dyn StorageEngine,
    child_table_def: &axiomdb_catalog::schema::TableDef,
    child_cols: &[axiomdb_catalog::schema::ColumnDef],
    child_col_idx: u16,
    fk_val: &Value,
    snap: TransactionSnapshot,
) -> Result<Vec<PkRowPair>, DbError> {
    use std::ops::Bound;
    let col_types = column_data_types(child_cols);
    let iter = axiomdb_storage::clustered_tree::range(
        storage,
        Some(child_table_def.root_page_id),
        Bound::Unbounded,
        Bound::Unbounded,
        &snap,
    )?;
    let mut result = Vec::new();
    for row in iter {
        let row = row?;
        let vals =
            crate::table::decode_row_from_bytes(&row.row_data, child_cols).or_else(|_| {
                // Overflow row: use the column_data_types path
                axiomdb_types::codec::decode_row(&row.row_data, &col_types)
            })?;
        let matches = vals
            .get(child_col_idx as usize)
            .map(|v| v == fk_val)
            .unwrap_or(false);
        if matches {
            result.push((row.key, vals));
        }
    }
    Ok(result)
}

/// Applies MVCC delete-marks to clustered rows identified by their PK keys.
fn delete_mark_clustered_rows(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_id: u32,
    root_pid: u64,
    pk_keys: &[Vec<u8>],
    snap: TransactionSnapshot,
) -> Result<(), DbError> {
    use axiomdb_storage::{clustered_leaf, clustered_tree};

    let txn_id = conn_txn.txn_id;

    for pk_key in pk_keys {
        let leaf_ref = clustered_tree::descend_to_leaf_pub(storage, root_pid, pk_key)?;
        let page_id = leaf_ref.header().page_id;
        let mut page = leaf_ref.into_page();

        let pos = match clustered_leaf::search(&page, pk_key) {
            Ok(pos) => pos,
            Err(_) => continue, // key not on this page — skip
        };

        let cell = clustered_leaf::read_cell(&page, pos as u16)?;
        if !cell.row_header.is_visible(&snap) {
            continue;
        }

        clustered_leaf::patch_txn_id_deleted(&mut page, pos, txn_id)?;
        page.update_checksum();
        storage.write_page(page_id, &page)?;
        txn.record_clustered_delete_mark_lightweight(
            conn_txn,
            table_id,
            root_pid,
            std::slice::from_ref(pk_key),
        )?;
    }

    Ok(())
}

/// Applies SET NULL to a single clustered row: delete-marks the old cell and
/// re-inserts the updated row, then maintains secondary indexes.
///
/// Returns the new effective clustered root (may differ if a split occurred).
#[allow(clippy::too_many_arguments)]
fn apply_clustered_set_null(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_id: u32,
    root_pid: u64,
    primary_idx: &IndexDef,
    secondary_idxs: &mut [IndexDef],
    child_cols: &[axiomdb_catalog::schema::ColumnDef],
    pk_key: &[u8],
    old_values: &[Value],
    new_values: &[Value],
    snap: TransactionSnapshot,
) -> Result<u64, DbError> {
    use axiomdb_storage::{clustered_leaf, clustered_tree};

    let txn_id = conn_txn.txn_id;

    // Step 1: Read the current row header so we can build the old_image for WAL.
    let old_image = {
        let leaf_ref = clustered_tree::descend_to_leaf_pub(storage, root_pid, pk_key)?;
        let page = leaf_ref.into_page();
        let pos = match clustered_leaf::search(&page, pk_key) {
            Ok(pos) => pos,
            Err(_) => return Ok(root_pid), // key not found — nothing to do
        };
        let cell = clustered_leaf::read_cell(&page, pos as u16)?;
        if !cell.row_header.is_visible(&snap) {
            return Ok(root_pid); // already invisible
        }
        ClusteredRowImage::new(root_pid, cell.row_header, cell.row_data)
    };

    // Step 2: Update the row in-place (PK unchanged — SET NULL only changes the FK column).
    // Using update_in_place / update_with_relocation avoids a duplicate-key error that
    // would occur if we delete-mark then re-insert at the same PK key.
    let col_types = column_data_types(child_cols);
    let encoded = encode_row(new_values, &col_types)?;
    let new_header = RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version: old_image.row_header.row_version.saturating_add(1),
        _flags: old_image.row_header._flags,
    };

    let new_root = match clustered_tree::update_in_place(
        storage,
        Some(root_pid),
        pk_key,
        &encoded,
        txn_id,
        &snap,
    ) {
        Ok(true) => root_pid,
        Ok(false) => return Ok(root_pid), // someone else updated this row
        Err(axiomdb_core::error::DbError::HeapPageFull { .. }) => {
            match clustered_tree::update_with_relocation(
                storage,
                Some(root_pid),
                pk_key,
                &encoded,
                txn_id,
                &snap,
            )? {
                Some(r) => r,
                None => return Ok(root_pid),
            }
        }
        Err(err) => return Err(err),
    };

    if new_root != root_pid {
        CatalogWriter::new(storage, txn, conn_txn)?.update_table_root(table_id, new_root)?;
    }
    let new_image = ClusteredRowImage::new(new_root, new_header, &encoded);
    txn.record_clustered_update(conn_txn, table_id, pk_key, &old_image, &new_image)?;

    // Step 3: Update clustered secondary indexes for the changed FK column.
    for idx in secondary_idxs.iter_mut() {
        let layout = match ClusteredSecondaryLayout::derive(idx, primary_idx) {
            Ok(l) => l,
            Err(_) => continue,
        };
        let root_atomic = AtomicU64::new(idx.root_page_id);
        let _ = layout.update_row(storage, &root_atomic, old_values, new_values)?;
        let new_idx_root = root_atomic.load(Ordering::Acquire);
        if new_idx_root != idx.root_page_id {
            CatalogWriter::new(storage, txn, conn_txn)?
                .update_index_root(idx.index_id, new_idx_root)?;
            idx.root_page_id = new_idx_root;
        }
    }

    Ok(new_root)
}

/// Resolves `(table_id, col_idx)` to `(table_name, column_name)` using the catalog.
///
/// Returns placeholder strings on catalog miss so error messages are always
/// human-readable even if the catalog is temporarily inconsistent.
pub(crate) fn resolve_names(
    storage: &dyn StorageEngine,
    snap: axiomdb_core::TransactionSnapshot,
    table_id: u32,
    col_idx: u16,
) -> (String, String) {
    let mut reader = match CatalogReader::new(storage, snap) {
        Ok(r) => r,
        Err(_) => return (format!("table#{table_id}"), format!("col#{col_idx}")),
    };
    let table_name = reader
        .get_table_by_id(table_id)
        .ok()
        .flatten()
        .map(|t| t.table_name)
        .unwrap_or_else(|| format!("table#{table_id}"));
    let col_name = reader
        .list_columns(table_id)
        .ok()
        .and_then(|cols| cols.into_iter().find(|c| c.col_idx == col_idx))
        .map(|c| c.name)
        .unwrap_or_else(|| format!("col#{col_idx}"));
    (table_name, col_name)
}
