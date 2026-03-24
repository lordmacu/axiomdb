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

use axiomdb_catalog::{schema::FkAction, CatalogReader, CatalogWriter, FkDef};
use axiomdb_core::{error::DbError, RecordId};
use axiomdb_index::BTree;
use axiomdb_storage::StorageEngine;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use crate::{bloom::BloomRegistry, key_encoding::encode_index_key, table::TableEngine};

/// Maximum ON DELETE CASCADE recursion depth.
/// Matches InnoDB's `FK_MAX_CASCADE_DEL`. Prevents infinite loops in circular graphs.
const MAX_CASCADE_DEPTH: u32 = 10;

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
    storage: &mut dyn StorageEngine,
    txn: &TxnManager,
    bloom: &mut BloomRegistry,
) -> Result<(), DbError> {
    if foreign_keys.is_empty() {
        return Ok(());
    }
    let _ = bloom; // Bloom not yet used for FK parent lookup — see TODO in module.

    let snap = txn.active_snapshot()?;

    for fk in foreign_keys {
        let fk_val = row.get(fk.child_col_idx as usize).unwrap_or(&Value::Null);

        // NULL FK → constraint passes (MATCH SIMPLE).
        if matches!(fk_val, Value::Null) {
            continue;
        }

        let key = encode_index_key(std::slice::from_ref(fk_val))?;

        // Find the parent's PRIMARY KEY or UNIQUE index covering parent_col_idx.
        // We use a block scope so the reader (which holds &storage) is dropped
        // before any call that needs &mut storage.
        let (is_primary, parent_index_root) = {
            let mut reader = CatalogReader::new(storage, snap)?;
            let parent_indexes = reader.list_indexes(fk.parent_table_id)?;
            let parent_idx = parent_indexes
                .iter()
                .find(|i| {
                    (i.is_primary || i.is_unique)
                        && i.columns.len() == 1
                        && i.columns[0].col_idx == fk.parent_col_idx
                })
                .ok_or_else(|| {
                    let (tname, cname) =
                        resolve_names(storage, snap, fk.parent_table_id, fk.parent_col_idx);
                    DbError::ForeignKeyNoParentIndex {
                        table: tname,
                        column: cname,
                    }
                })?;
            (parent_idx.is_primary, parent_idx.root_page_id)
        }; // reader dropped here → &storage released

        // Look up the parent key.
        //
        // Primary key B-Tree indexes are created empty and are NOT populated
        // during INSERT (insert_into_indexes skips primary indexes in Phase 6.5).
        // For PK indexes → fall back to a full parent table scan.
        //
        // Non-primary UNIQUE indexes are populated via insert_into_indexes →
        // B-Tree point lookup is correct and efficient.
        //
        // TODO Phase 6.9: populate PK B-Trees via insert_into_indexes and use
        //   B-Tree + Bloom shortcut for all index types.
        let parent_exists = if is_primary {
            // Load parent table def + cols, then scan.
            let (parent_table_def, parent_cols) = {
                let mut reader = CatalogReader::new(storage, snap)?;
                let tdef = reader.get_table_by_id(fk.parent_table_id)?.ok_or(
                    DbError::CatalogTableNotFound {
                        table_id: fk.parent_table_id,
                    },
                )?;
                let pcols = reader.list_columns(fk.parent_table_id)?;
                (tdef, pcols)
            }; // reader dropped
            let rows =
                TableEngine::scan_table(storage, &parent_table_def, &parent_cols, snap, None)?;
            rows.iter().any(|(_, row)| {
                row.get(fk.parent_col_idx as usize)
                    .map(|v| v == fk_val)
                    .unwrap_or(false)
            })
        } else {
            // Non-primary UNIQUE index — populated via insert_into_indexes.
            BTree::lookup_in(storage, parent_index_root, &key)?.is_some()
        };

        if !parent_exists {
            let (tname, cname) = resolve_names(storage, snap, fk.child_table_id, fk.child_col_idx);
            return Err(DbError::ForeignKeyViolation {
                table: tname,
                column: cname,
                value: format!("{fk_val}"),
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
    storage: &mut dyn StorageEngine,
    txn: &TxnManager,
    bloom: &mut BloomRegistry,
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

    check_fk_child_insert(new_row, &changed_fks, storage, txn, bloom)
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
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
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

    let snap = txn.active_snapshot()?;

    // Load all FK constraints referencing this table as parent.
    let fk_list = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.list_fk_constraints_referencing(parent_table_id)?
    };
    if fk_list.is_empty() {
        return Ok(());
    }

    for fk in &fk_list {
        // Load child table metadata.
        let child_table_def = {
            let mut reader = CatalogReader::new(storage, snap)?;
            reader
                .get_table_by_id(fk.child_table_id)?
                .ok_or(DbError::CatalogTableNotFound {
                    table_id: fk.child_table_id,
                })?
        };
        let child_cols = {
            let mut reader = CatalogReader::new(storage, snap)?;
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

        // Find the FK index on the child (for RESTRICT fast path).
        let fk_index_root: Option<u64> = {
            let mut reader = CatalogReader::new(storage, snap)?;
            let child_indexes = reader.list_indexes(fk.child_table_id)?;
            child_indexes
                .iter()
                .find(|i| {
                    (fk.fk_index_id != 0 && i.index_id == fk.fk_index_id)
                        || (!i.columns.is_empty() && i.columns[0].col_idx == fk.child_col_idx)
                })
                .map(|i| i.root_page_id)
        };

        for (_, parent_row) in deleted_rows {
            let parent_key_val = parent_row
                .get(fk.parent_col_idx as usize)
                .unwrap_or(&Value::Null);

            // Parent key is NULL → no child can reference NULL.
            if matches!(parent_key_val, Value::Null) {
                continue;
            }

            let parent_key = encode_index_key(std::slice::from_ref(parent_key_val))?;

            match fk.on_delete {
                FkAction::NoAction | FkAction::Restrict => {
                    // Fast path: use FK index for existence check if available.
                    let has_child = if let Some(root) = fk_index_root {
                        BTree::lookup_in(storage, root, &parent_key)?.is_some()
                    } else {
                        // Slow path: full scan.
                        children_exist_via_scan(
                            storage,
                            &child_table_def,
                            &child_cols,
                            fk.child_col_idx,
                            parent_key_val,
                            snap,
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
                    // Full scan to find ALL children (BTree may miss duplicates).
                    let child_rows = find_children_via_scan(
                        storage,
                        &child_table_def,
                        &child_cols,
                        fk.child_col_idx,
                        parent_key_val,
                        snap,
                    )?;

                    if child_rows.is_empty() {
                        continue;
                    }

                    // Recursively enforce FK on children's children BEFORE deleting.
                    enforce_fk_on_parent_delete(
                        &child_rows,
                        fk.child_table_id,
                        storage,
                        txn,
                        bloom,
                        depth + 1,
                    )?;

                    // Batch-delete children from the heap.
                    let child_rids: Vec<RecordId> =
                        child_rows.iter().map(|(rid, _)| *rid).collect();
                    crate::table::TableEngine::delete_rows_batch(
                        storage,
                        txn,
                        &child_table_def,
                        &child_rids,
                    )?;

                    // Maintain secondary indexes on the child table.
                    let secondary_indexes = {
                        let mut reader = CatalogReader::new(storage, snap)?;
                        let all = reader.list_indexes(fk.child_table_id)?;
                        all.into_iter()
                            .filter(|i| !i.is_primary && !i.columns.is_empty())
                            .collect::<Vec<_>>()
                    };
                    if !secondary_indexes.is_empty() {
                        for (_, child_row_vals) in &child_rows {
                            let updated = crate::index_maintenance::delete_from_indexes(
                                &secondary_indexes,
                                child_row_vals,
                                storage,
                                bloom,
                                &[],
                            )?;
                            for (index_id, new_root) in updated {
                                CatalogWriter::new(storage, txn)?
                                    .update_index_root(index_id, new_root)?;
                            }
                        }
                    }
                }

                FkAction::SetNull => {
                    // Full scan to find ALL children.
                    let child_rows = find_children_via_scan(
                        storage,
                        &child_table_def,
                        &child_cols,
                        fk.child_col_idx,
                        parent_key_val,
                        snap,
                    )?;

                    if child_rows.is_empty() {
                        continue;
                    }

                    let secondary_indexes = {
                        let mut reader = CatalogReader::new(storage, snap)?;
                        let all = reader.list_indexes(fk.child_table_id)?;
                        all.into_iter()
                            .filter(|i| !i.is_primary && !i.columns.is_empty())
                            .collect::<Vec<_>>()
                    };

                    for (child_rid, child_row) in &child_rows {
                        // Delete old FK key from secondary indexes before update.
                        if !secondary_indexes.is_empty() {
                            let del_updated = crate::index_maintenance::delete_from_indexes(
                                &secondary_indexes,
                                child_row,
                                storage,
                                bloom,
                                &[],
                            )?;
                            for (index_id, new_root) in del_updated {
                                CatalogWriter::new(storage, txn)?
                                    .update_index_root(index_id, new_root)?;
                            }
                        }

                        // Set FK column to NULL in child row.
                        let mut new_child_row = child_row.clone();
                        new_child_row[fk.child_col_idx as usize] = Value::Null;

                        TableEngine::update_row(
                            storage,
                            txn,
                            &child_table_def,
                            &child_cols,
                            *child_rid,
                            new_child_row,
                        )?;
                        // NULL is not inserted into the FK index — no bloom.add() needed.
                        // The FK index entry for the old value still exists in the BTree
                        // (pointing to the old RecordId which is now dead). This is OK:
                        // dead RecordIds are never returned by scan_visible. The FK index
                        // is non-unique so the BTree entry becomes a stale pointer that
                        // will be cleaned up by VACUUM (Phase 9).
                        bloom.mark_dirty(fk.fk_index_id);
                    }
                }

                FkAction::SetDefault => {
                    return Err(DbError::NotImplemented {
                        feature: "ON DELETE SET DEFAULT — Phase 6.9".into(),
                    });
                }
            }
        }
    }

    Ok(())
}

/// Enforces FK constraints when the referenced parent key columns are updated.
///
/// Only RESTRICT / NO ACTION are supported. CASCADE / SET NULL on UPDATE are
/// deferred to Phase 6.9.
pub fn enforce_fk_on_parent_update(
    old_rows: &[(RecordId, Vec<Value>)],
    new_values_per_row: &[Vec<Value>],
    parent_table_id: u32,
    storage: &mut dyn StorageEngine,
    txn: &TxnManager,
) -> Result<(), DbError> {
    if old_rows.is_empty() {
        return Ok(());
    }

    let snap = txn.active_snapshot()?;
    let fk_list = {
        let mut reader = CatalogReader::new(storage, snap)?;
        reader.list_fk_constraints_referencing(parent_table_id)?
    };
    if fk_list.is_empty() {
        return Ok(());
    }

    for fk in &fk_list {
        let child_table_def = {
            let mut reader = CatalogReader::new(storage, snap)?;
            reader
                .get_table_by_id(fk.child_table_id)?
                .ok_or(DbError::CatalogTableNotFound {
                    table_id: fk.child_table_id,
                })?
        };
        let child_cols = {
            let mut reader = CatalogReader::new(storage, snap)?;
            reader.list_columns(fk.child_table_id)?
        };
        let child_col_name = child_cols
            .iter()
            .find(|c| c.col_idx == fk.child_col_idx)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| format!("col_{}", fk.child_col_idx));

        let fk_index_root: Option<u64> = {
            let mut reader = CatalogReader::new(storage, snap)?;
            let child_indexes = reader.list_indexes(fk.child_table_id)?;
            child_indexes
                .iter()
                .find(|i| {
                    (fk.fk_index_id != 0 && i.index_id == fk.fk_index_id)
                        || (!i.columns.is_empty() && i.columns[0].col_idx == fk.child_col_idx)
                })
                .map(|i| i.root_page_id)
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

            let old_parent_key = encode_index_key(std::slice::from_ref(old_key_val))?;

            let has_children = if let Some(root) = fk_index_root {
                BTree::lookup_in(storage, root, &old_parent_key)?.is_some()
            } else {
                children_exist_via_scan(
                    storage,
                    &child_table_def,
                    &child_cols,
                    fk.child_col_idx,
                    old_key_val,
                    snap,
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
                    _ => {
                        return Err(DbError::NotImplemented {
                            feature: "ON UPDATE CASCADE / SET NULL / SET DEFAULT — Phase 6.9"
                                .into(),
                        });
                    }
                }
            }
        }
    }

    Ok(())
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Returns `true` if any child row has FK column equal to `fk_val` (full scan).
fn children_exist_via_scan(
    storage: &mut dyn StorageEngine,
    child_table_def: &axiomdb_catalog::schema::TableDef,
    child_cols: &[axiomdb_catalog::schema::ColumnDef],
    child_col_idx: u16,
    fk_val: &Value,
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<bool, DbError> {
    let rows = TableEngine::scan_table(storage, child_table_def, child_cols, snap, None)?;
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
    storage: &mut dyn StorageEngine,
    child_table_def: &axiomdb_catalog::schema::TableDef,
    child_cols: &[axiomdb_catalog::schema::ColumnDef],
    child_col_idx: u16,
    fk_val: &Value,
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    let rows = TableEngine::scan_table(storage, child_table_def, child_cols, snap, None)?;
    Ok(rows
        .into_iter()
        .filter(|(_, row)| {
            row.get(child_col_idx as usize)
                .map(|v| v == fk_val)
                .unwrap_or(false)
        })
        .collect())
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
