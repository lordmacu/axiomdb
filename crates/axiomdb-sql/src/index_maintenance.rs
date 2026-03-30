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

use axiomdb_index::page_layout::encode_rid;

use axiomdb_storage::heap_chain::HeapChain;

use crate::{eval::eval, eval::is_truthy, expr::Expr, key_encoding::encode_index_key};

/// Phase 7.3b — Check if a duplicate key in a unique index points to a VISIBLE row.
///
/// With MVCC lazy index deletion, dead entries (deleted or rolled-back rows) remain
/// in the index. A uniqueness violation only occurs if the existing entry points to
/// a row that is visible under the current snapshot.
///
/// Returns `true` if a visible duplicate exists (should raise UniqueViolation).
fn has_visible_duplicate(
    storage: &dyn StorageEngine,
    root_page_id: u64,
    key: &[u8],
    snap: TransactionSnapshot,
) -> Result<bool, DbError> {
    match BTree::lookup_in(storage, root_page_id, key)? {
        None => Ok(false),
        Some(existing_rid) => {
            // Check heap visibility — only visible rows cause a violation.
            HeapChain::is_slot_visible(storage, existing_rid.page_id, existing_rid.slot_id, snap)
        }
    }
}

// ── FK composite key helpers ──────────────────────────────────────────────────

/// Builds the B-Tree key for an FK auto-index entry (Phase 6.9).
///
/// Format: `encode_index_key(&[fk_val])` ++ `encode_rid(rid)` (10 bytes).
/// Every entry is globally unique even when multiple rows share the same `fk_val`,
/// following InnoDB's approach of appending the primary key as a tiebreaker.
pub fn fk_composite_key(fk_val: &axiomdb_types::Value, rid: RecordId) -> Result<Vec<u8>, DbError> {
    let mut key = encode_index_key(std::slice::from_ref(fk_val))?;
    key.extend_from_slice(&encode_rid(rid));
    Ok(key)
}

/// Returns `(lo, hi)` bounds for `BTree::range_in` to find all FK index entries
/// with a given `fk_val`, regardless of which RecordId they point to.
///
/// `lo = prefix + [0x00; 10]` — smallest possible RecordId suffix.
/// `hi = prefix + [0xFF; 10]` — largest possible RecordId suffix.
pub fn fk_key_range(fk_val: &axiomdb_types::Value) -> Result<(Vec<u8>, Vec<u8>), DbError> {
    let prefix = encode_index_key(std::slice::from_ref(fk_val))?;
    let mut lo = prefix.clone();
    lo.extend_from_slice(&[0u8; 10]);
    let mut hi = prefix;
    hi.extend_from_slice(&[0xFF; 10]);
    Ok((lo, hi))
}

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
    snap: TransactionSnapshot,
) -> Result<Vec<(u32, u64)>, DbError> {
    let mut updated_roots = Vec::new();

    for (i, idx) in indexes
        .iter()
        .enumerate()
        .filter(|(_, i)| !i.columns.is_empty())
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

        // Key encoding:
        // - FK auto-indexes and non-unique indexes: key || encode_rid(rid) (10 bytes).
        //   Makes every B-Tree entry globally unique even when multiple rows share the
        //   same indexed value (InnoDB approach — Phase 6.9 for FK; generalized here).
        // - Unique indexes: plain encode_index_key (duplicate → UniqueViolation above).
        let key = if idx.is_fk_index || !idx.is_unique {
            let mut k = encode_index_key(&key_vals)?;
            k.extend_from_slice(&encode_rid(rid));
            k
        } else {
            encode_index_key(&key_vals)?
        };

        // Uniqueness check — skip for FK auto-indexes (never unique by FK semantics).
        // Phase 7.3b: check heap visibility for existing entry — dead entries don't
        // count as duplicates (they'll be cleaned by vacuum).
        if idx.is_unique
            && !idx.is_fk_index
            && has_visible_duplicate(storage, idx.root_page_id, &key, snap)?
        {
            let dup_val = key_vals.first().map(|v| format!("{v}"));
            return Err(DbError::UniqueViolation {
                index_name: idx.name.clone(),
                value: dup_val,
            });
        }

        let root_pid = AtomicU64::new(idx.root_page_id);
        BTree::insert_in(storage, &root_pid, &key, rid, idx.fillfactor)?;
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
    rid: RecordId,
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
    compiled_preds: &[Option<Expr>],
) -> Result<Vec<(u32, u64)>, DbError> {
    let mut updated_roots = Vec::new();

    for (i, idx) in indexes
        .iter()
        .enumerate()
        .filter(|(_, i)| !i.columns.is_empty())
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

        // FK auto-indexes and non-unique indexes: key || encode_rid(rid).
        // Unique indexes: plain encode_index_key.
        let key = if idx.is_fk_index || !idx.is_unique {
            let base = match encode_index_key(&key_vals) {
                Ok(k) => k,
                Err(DbError::IndexKeyTooLong { .. }) => continue,
                Err(e) => return Err(e),
            };
            let mut k = base;
            k.extend_from_slice(&encode_rid(rid));
            k
        } else {
            match encode_index_key(&key_vals) {
                Ok(k) => k,
                Err(DbError::IndexKeyTooLong { .. }) => continue,
                Err(e) => return Err(e),
            }
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

// ── Batch delete helpers (Phase 5.19) ─────────────────────────────────────────

/// For each index in `indexes`, encode the delete key for every row in `rows`
/// using the same rules as `delete_from_indexes` (NULL skip, partial predicate,
/// unique/non-unique encoding). Returns one sorted `Vec<Vec<u8>>` per index.
pub fn collect_delete_keys_by_index(
    indexes: &[IndexDef],
    rows: &[(RecordId, Vec<Value>)],
    compiled_preds: &[Option<Expr>],
) -> Result<Vec<Vec<Vec<u8>>>, DbError> {
    let mut buckets: Vec<Vec<Vec<u8>>> = vec![Vec::new(); indexes.len()];

    for (i, idx) in indexes
        .iter()
        .enumerate()
        .filter(|(_, i)| !i.columns.is_empty())
    {
        let pred = compiled_preds.get(i).and_then(|p| p.as_ref());
        for (rid, row) in rows {
            if let Some(p) = pred {
                if !is_truthy(&eval(p, row)?) {
                    continue;
                }
            }
            let key_vals: Vec<Value> = idx
                .columns
                .iter()
                .map(|c| row.get(c.col_idx as usize).cloned().unwrap_or(Value::Null))
                .collect();
            if key_vals.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            let key = if idx.is_fk_index || !idx.is_unique {
                let base = match encode_index_key(&key_vals) {
                    Ok(k) => k,
                    Err(DbError::IndexKeyTooLong { .. }) => continue,
                    Err(e) => return Err(e),
                };
                let mut k = base;
                k.extend_from_slice(&encode_rid(*rid));
                k
            } else {
                match encode_index_key(&key_vals) {
                    Ok(k) => k,
                    Err(DbError::IndexKeyTooLong { .. }) => continue,
                    Err(e) => return Err(e),
                }
            };
            buckets[i].push(key);
        }
        buckets[i].sort_unstable();
    }

    Ok(buckets)
}

/// Removes all keys in `key_buckets[i]` from `indexes[i]` using one
/// `BTree::delete_many_in` call per index. Updates `indexes[i].root_page_id`
/// in place and returns `(index_id, new_root)` for every index whose root changed.
///
/// `key_buckets` must be parallel to `indexes` and each bucket pre-sorted ascending.
pub fn delete_many_from_indexes(
    indexes: &mut [IndexDef],
    key_buckets: Vec<Vec<Vec<u8>>>,
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<Vec<(u32, u64)>, DbError> {
    let mut updated_roots: Vec<(u32, u64)> = Vec::new();

    for (i, idx) in indexes.iter_mut().enumerate() {
        if idx.columns.is_empty() {
            continue;
        }
        let keys = match key_buckets.get(i) {
            Some(k) if !k.is_empty() => k,
            _ => continue,
        };
        let root_pid = AtomicU64::new(idx.root_page_id);
        BTree::delete_many_in(storage, &root_pid, keys)?;
        bloom.mark_dirty(idx.index_id);
        let new_root = root_pid.load(Ordering::Acquire);
        if new_root != idx.root_page_id {
            idx.root_page_id = new_root;
            updated_roots.push((idx.index_id, new_root));
        }
    }

    Ok(updated_roots)
}

pub(crate) fn index_key_values_if_indexed(
    idx: &IndexDef,
    row: &[Value],
    compiled_pred: Option<&Expr>,
) -> Result<Option<Vec<Value>>, DbError> {
    if let Some(pred) = compiled_pred {
        if !is_truthy(&eval(pred, row)?) {
            return Ok(None);
        }
    }

    let key_vals: Vec<Value> = idx
        .columns
        .iter()
        .map(|c| row.get(c.col_idx as usize).cloned().unwrap_or(Value::Null))
        .collect();
    if key_vals.iter().any(|v| matches!(v, Value::Null)) {
        return Ok(None);
    }
    Ok(Some(key_vals))
}

pub(crate) fn encode_index_entry_key(
    idx: &IndexDef,
    key_vals: &[Value],
    rid: RecordId,
) -> Result<Vec<u8>, DbError> {
    if idx.is_fk_index || !idx.is_unique {
        let mut key = encode_index_key(key_vals)?;
        key.extend_from_slice(&encode_rid(rid));
        Ok(key)
    } else {
        encode_index_key(key_vals)
    }
}

/// Returns `true` if updating `(old_row, old_rid)` to `(new_row, new_rid)` requires
/// maintenance for `idx`.
///
/// If the RID changes, the index is always affected. When the RID is stable, the
/// index is affected only if its membership or logical key changes.
pub fn update_affects_index(
    idx: &IndexDef,
    compiled_pred: Option<&Expr>,
    old_row: &[Value],
    old_rid: RecordId,
    new_row: &[Value],
    new_rid: RecordId,
) -> Result<bool, DbError> {
    if old_rid != new_rid {
        return Ok(true);
    }

    let old_key_vals = index_key_values_if_indexed(idx, old_row, compiled_pred)?;
    let new_key_vals = index_key_values_if_indexed(idx, new_row, compiled_pred)?;
    Ok(match (old_key_vals, new_key_vals) {
        (None, None) => false,
        (Some(old_vals), Some(new_vals)) => old_vals != new_vals,
        _ => true,
    })
}

/// Removes all `keys` from a single index with one `delete_many_in` call.
pub fn delete_many_from_single_index(
    idx: &mut IndexDef,
    keys: &[Vec<u8>],
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<Option<u64>, DbError> {
    if idx.columns.is_empty() || keys.is_empty() {
        return Ok(None);
    }

    let root_pid = AtomicU64::new(idx.root_page_id);
    BTree::delete_many_in(storage, &root_pid, keys)?;
    bloom.mark_dirty(idx.index_id);
    let new_root = root_pid.load(Ordering::Acquire);
    if new_root != idx.root_page_id {
        idx.root_page_id = new_root;
        Ok(Some(new_root))
    } else {
        Ok(None)
    }
}

// ── Batch insert helpers (Phase 5.21) ─────────────────────────────────────────

/// Inserts all rows in `rows` into every secondary index, persisting each
/// changed root **once per index per flush** instead of once per row.
///
/// Per index, the function walks all `(row, rid)` pairs and accumulates root
/// changes through splits. The final root is written to the catalog exactly
/// once via `CatalogWriter::update_index_root`, which eliminates the N catalog
/// writes that the per-row path would produce.
///
/// When `skip_unique_check` is `true`, the B-Tree uniqueness lookup is
/// skipped entirely. This is safe when the caller has already verified
/// uniqueness (against committed data AND intra-batch via `unique_seen`)
/// at enqueue time — as the staged-insert path does. Eliminating the
/// redundant N lookups at flush time halves total B-Tree operations.
///
/// Returns `(index_id, new_root_page_id)` for every index whose root changed.
/// The caller is responsible for updating the in-memory `IndexDef` slice.
#[allow(clippy::too_many_arguments)]
pub fn batch_insert_into_indexes(
    indexes: &mut [IndexDef],
    rows: &[Vec<Value>],
    rids: &[RecordId],
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
    compiled_preds: &[Option<Expr>],
    skip_unique_check: bool,
    committed_empty: &std::collections::HashSet<u32>,
    snap: TransactionSnapshot,
) -> Result<Vec<(u32, u64)>, DbError> {
    debug_assert_eq!(
        rows.len(),
        rids.len(),
        "batch_insert_into_indexes: rows and rids must be parallel"
    );
    if rows.is_empty() {
        return Ok(Vec::new());
    }

    let mut updated_roots: Vec<(u32, u64)> = Vec::new();

    for (i, idx) in indexes.iter_mut().enumerate() {
        if idx.columns.is_empty() {
            continue;
        }

        let pred = compiled_preds.get(i).and_then(|p| p.as_ref());

        // ── Collect (encoded_key, rid) for this index ────────────────────────
        let mut pairs: Vec<(Vec<u8>, RecordId)> = Vec::new();
        for (row, rid) in rows.iter().zip(rids.iter()) {
            if let Some(p) = pred {
                if !is_truthy(&eval(p, row)?) {
                    continue;
                }
            }
            let key_vals: Vec<Value> = idx
                .columns
                .iter()
                .map(|c| row.get(c.col_idx as usize).cloned().unwrap_or(Value::Null))
                .collect();
            if key_vals.iter().any(|v| matches!(v, Value::Null)) {
                continue;
            }
            let key = if idx.is_fk_index || !idx.is_unique {
                let mut k = encode_index_key(&key_vals)?;
                k.extend_from_slice(&encode_rid(*rid));
                k
            } else {
                encode_index_key(&key_vals)?
            };
            pairs.push((key, *rid));
        }

        if pairs.is_empty() {
            continue;
        }

        // ── Bulk load path: empty committed index → build from scratch ───────
        if committed_empty.contains(&idx.index_id) {
            pairs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let refs: Vec<(&[u8], RecordId)> =
                pairs.iter().map(|(k, r)| (k.as_slice(), *r)).collect();
            let new_root =
                BTree::bulk_load_sorted(storage, idx.root_page_id, &refs, idx.fillfactor)?;
            for (k, _) in &pairs {
                bloom.add(idx.index_id, k);
            }
            idx.root_page_id = new_root;
            updated_roots.push((idx.index_id, new_root));
            continue;
        }

        // ── Per-row insert path: non-empty committed index ───────────────────
        let original_root = idx.root_page_id;
        let root_pid = AtomicU64::new(original_root);

        for (key, rid) in &pairs {
            if !skip_unique_check
                && idx.is_unique
                && !idx.is_fk_index
                && has_visible_duplicate(storage, root_pid.load(Ordering::Acquire), key, snap)?
            {
                let dup_val = Some("(encoded)".to_string());
                return Err(DbError::UniqueViolation {
                    index_name: idx.name.clone(),
                    value: dup_val,
                });
            }
            BTree::insert_in(storage, &root_pid, key, *rid, idx.fillfactor)?;
            bloom.add(idx.index_id, key);
        }

        let new_root = root_pid.load(Ordering::Acquire);
        if new_root != original_root {
            idx.root_page_id = new_root;
            updated_roots.push((idx.index_id, new_root));
        }
    }

    Ok(updated_roots)
}

/// Inserts multiple rows into a single index in one pass, persisting the root
/// once after all insertions. Returns the new root if it changed.
///
/// Mirrors `batch_insert_into_indexes` but operates on a single index and
/// does not use the bulk-load path (UPDATE rows go into an existing non-empty index).
pub fn insert_many_into_single_index(
    idx: &mut IndexDef,
    compiled_pred: Option<&Expr>,
    rows: &[(&[Value], RecordId)],
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
    snap: TransactionSnapshot,
) -> Result<Option<u64>, DbError> {
    if idx.columns.is_empty() || rows.is_empty() {
        return Ok(None);
    }

    let original_root = idx.root_page_id;
    let root_pid = AtomicU64::new(original_root);

    for (row, rid) in rows {
        let Some(key_vals) = index_key_values_if_indexed(idx, row, compiled_pred)? else {
            continue;
        };
        let key = encode_index_entry_key(idx, &key_vals, *rid)?;

        if idx.is_unique
            && !idx.is_fk_index
            && has_visible_duplicate(storage, root_pid.load(Ordering::Acquire), &key, snap)?
        {
            let dup_val = key_vals.first().map(|v| format!("{v}"));
            return Err(DbError::UniqueViolation {
                index_name: idx.name.clone(),
                value: dup_val,
            });
        }

        BTree::insert_in(storage, &root_pid, &key, *rid, idx.fillfactor)?;
        bloom.add(idx.index_id, &key);
    }

    let new_root = root_pid.load(Ordering::Acquire);
    if new_root != original_root {
        idx.root_page_id = new_root;
        Ok(Some(new_root))
    } else {
        Ok(None)
    }
}

/// Inserts one row into a single index and returns the new root if it changed.
pub fn insert_into_single_index(
    idx: &mut IndexDef,
    compiled_pred: Option<&Expr>,
    row: &[Value],
    rid: RecordId,
    storage: &mut dyn StorageEngine,
    bloom: &mut crate::bloom::BloomRegistry,
    snap: TransactionSnapshot,
) -> Result<Option<u64>, DbError> {
    if idx.columns.is_empty() {
        return Ok(None);
    }

    let Some(key_vals) = index_key_values_if_indexed(idx, row, compiled_pred)? else {
        return Ok(None);
    };
    let key = encode_index_entry_key(idx, &key_vals, rid)?;

    if idx.is_unique
        && !idx.is_fk_index
        && has_visible_duplicate(storage, idx.root_page_id, &key, snap)?
    {
        let dup_val = key_vals.first().map(|v| format!("{v}"));
        return Err(DbError::UniqueViolation {
            index_name: idx.name.clone(),
            value: dup_val,
        });
    }

    let root_pid = AtomicU64::new(idx.root_page_id);
    BTree::insert_in(storage, &root_pid, &key, rid, idx.fillfactor)?;
    bloom.add(idx.index_id, &key);
    let new_root = root_pid.load(Ordering::Acquire);
    if new_root != idx.root_page_id {
        idx.root_page_id = new_root;
        Ok(Some(new_root))
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_catalog::{IndexColumnDef, SortOrder};

    fn make_index(col_idx: u16) -> IndexDef {
        IndexDef {
            index_id: 1,
            table_id: 1,
            name: "idx_test".to_string(),
            root_page_id: 10,
            is_unique: false,
            is_primary: false,
            columns: vec![IndexColumnDef {
                col_idx,
                order: SortOrder::Asc,
            }],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
        }
    }

    #[test]
    fn test_update_affects_index_false_when_rid_and_key_stay_stable() {
        let idx = make_index(0);
        let old_rid = RecordId {
            page_id: 42,
            slot_id: 3,
        };
        let new_rid = old_rid;
        let old_row = vec![Value::Int(7), Value::Int(10)];
        let new_row = vec![Value::Int(7), Value::Int(99)];

        assert!(
            !update_affects_index(&idx, None, &old_row, old_rid, &new_row, new_rid).unwrap(),
            "non-indexed column change must not affect index when RID stays stable"
        );
    }

    #[test]
    fn test_update_affects_index_true_when_rid_changes_even_if_key_does_not() {
        let idx = make_index(0);
        let old_row = vec![Value::Int(7), Value::Int(10)];
        let new_row = vec![Value::Int(7), Value::Int(99)];

        assert!(
            update_affects_index(
                &idx,
                None,
                &old_row,
                RecordId {
                    page_id: 42,
                    slot_id: 3,
                },
                &new_row,
                RecordId {
                    page_id: 84,
                    slot_id: 1,
                },
            )
            .unwrap(),
            "fallback delete+insert rows must still treat the index as affected"
        );
    }

    #[test]
    fn test_update_affects_index_true_when_partial_predicate_membership_changes() {
        let mut idx = make_index(0);
        idx.predicate = Some("active = true".to_string());
        let predicate = Expr::BinaryOp {
            op: crate::expr::BinaryOp::Eq,
            left: Box::new(Expr::Column {
                col_idx: 1,
                name: "active".to_string(),
            }),
            right: Box::new(Expr::Literal(Value::Bool(true))),
        };
        let rid = RecordId {
            page_id: 42,
            slot_id: 3,
        };
        let old_row = vec![Value::Int(7), Value::Bool(true)];
        let new_row = vec![Value::Int(7), Value::Bool(false)];

        assert!(
            update_affects_index(&idx, Some(&predicate), &old_row, rid, &new_row, rid).unwrap(),
            "partial index membership changes must force maintenance even with stable RID"
        );
    }
}
