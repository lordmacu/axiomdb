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

use axiomdb_catalog::{CatalogReader, IndexColumnDef, IndexDef};
use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot};
use axiomdb_index::BTree;
use axiomdb_storage::StorageEngine;
use axiomdb_types::Value;

use axiomdb_index::page_layout::{encode_rid, MAX_KEY_LEN};

use axiomdb_storage::heap_chain::HeapChain;

use crate::{
    eval::eval,
    eval::is_truthy,
    expr::Expr,
    key_encoding::{decode_index_key, encode_index_key, MAX_INDEX_KEY},
};

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

// ── GIN JSONB key helpers ───────────────────────────────────────────────────

const GIN_SEPARATOR: u8 = 0x00;

/// Dummy B-Tree payload for clustered GIN entries.
///
/// The real bookmark is the clustered primary-key suffix encoded in the key:
/// `[term][0x00][pk_key]`.
pub(crate) const GIN_CLUSTERED_DUMMY_RID: RecordId = RecordId {
    page_id: 0,
    slot_id: 0,
};

fn dedup_gin_terms(mut terms: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
    terms.sort_unstable();
    terms.dedup();
    terms
}

/// Extracts all leaf elements from a `Value::Array` and encodes each as a GIN key.
///
/// Format per element: `[ColumnType tag:u8][encoded element bytes]`.
///
/// - NULL elements are skipped (not indexed).
/// - Nested arrays are flattened recursively.
/// - Results are deduplicated and sorted.
///
/// This mirrors the PostgreSQL `gin_extract_array` function for `text[]` ops.
/// The encoding uses ColumnType tags (1-13) to identify element types,
/// which aligns with how JSONB GIN extraction uses type-specific flags.
pub(crate) fn gin_extract_array_keys(arr: &[Value]) -> Vec<Vec<u8>> {
    let mut terms: Vec<Vec<u8>> = Vec::new();
    flatten_array_elements(arr, &mut terms);
    dedup_gin_terms(terms)
}

/// Recursively flattens array elements into GIN key bytes.
fn flatten_array_elements(arr: &[Value], out: &mut Vec<Vec<u8>>) {
    for elem in arr {
        match elem {
            Value::Null => {
                // NULL elements are not indexed (skip)
            }
            Value::Bool(b) => {
                out.push(vec![1u8, *b as u8]);
            }
            Value::Int(n) => {
                // ColumnType::Int = 2, followed by 4-byte big-endian
                let mut t = Vec::with_capacity(5);
                t.push(2u8); // ColumnType::Int
                t.extend_from_slice(&n.to_be_bytes());
                out.push(t);
            }
            Value::BigInt(n) => {
                // ColumnType::BigInt = 3, followed by 8-byte big-endian
                let mut t = Vec::with_capacity(9);
                t.push(3u8); // ColumnType::BigInt
                t.extend_from_slice(&n.to_be_bytes());
                out.push(t);
            }
            Value::Real(f) => {
                // ColumnType::Float = 4, followed by 8-byte IEEE 754 big-endian
                let mut t = Vec::with_capacity(9);
                t.push(4u8); // ColumnType::Float
                t.extend_from_slice(&f.to_be_bytes());
                out.push(t);
            }
            Value::Text(s) => {
                // ColumnType::Text = 5, followed by length-prefixed UTF-8 bytes
                let mut t = Vec::with_capacity(1 + 4 + s.len());
                t.push(5u8); // ColumnType::Text
                t.extend_from_slice(&(s.len() as u32).to_be_bytes());
                t.extend_from_slice(s.as_bytes());
                out.push(t);
            }
            Value::Json(s) => {
                // JSON strings encoded same as Text (ColumnType::Json = 9)
                let mut t = Vec::with_capacity(1 + 4 + s.len());
                t.push(9u8); // ColumnType::Json
                t.extend_from_slice(&(s.len() as u32).to_be_bytes());
                t.extend_from_slice(s.as_bytes());
                out.push(t);
            }
            Value::Uuid(u) => {
                // ColumnType::Uuid = 8, followed by 16 raw bytes
                let mut t = Vec::with_capacity(17);
                t.push(8u8); // ColumnType::Uuid
                t.extend_from_slice(u);
                out.push(t);
            }
            Value::Date(days) => {
                // ColumnType::Date = 12, followed by 4-byte big-endian i32
                let mut t = Vec::with_capacity(5);
                t.push(12u8); // ColumnType::Date
                t.extend_from_slice(&days.to_be_bytes());
                out.push(t);
            }
            Value::Timestamp(micros) => {
                // ColumnType::Timestamp = 7, followed by 8-byte big-endian i64
                let mut t = Vec::with_capacity(9);
                t.push(7u8); // ColumnType::Timestamp
                t.extend_from_slice(&micros.to_be_bytes());
                out.push(t);
            }
            Value::Bytes(b) => {
                // ColumnType::Bytes = 6, followed by length-prefixed bytes
                let mut t = Vec::with_capacity(1 + 4 + b.len());
                t.push(6u8); // ColumnType::Bytes
                t.extend_from_slice(&(b.len() as u32).to_be_bytes());
                t.extend_from_slice(b);
                out.push(t);
            }
            Value::Decimal(mantissa, scale) => {
                // ColumnType::Decimal = 11
                // 16-byte i128 mantissa (big-endian) + 1-byte scale
                let mut t = Vec::with_capacity(18);
                t.push(11u8); // ColumnType::Decimal
                t.extend_from_slice(&mantissa.to_be_bytes());
                t.push(*scale);
                out.push(t);
            }
            Value::Jsonb(_) => {
                // JSONB elements not typically in SQL arrays, but handle via JSON text
                // This shouldn't happen in practice since JSONB is stored separately
            }
            Value::Array(nested) => {
                // Flatten nested arrays recursively
                flatten_array_elements(nested, out);
            }
        }
    }
}

/// Returns GIN key terms for a row value, handling JSONB, JSON text, and Arrays.
///
/// - JSONB: uses `axiomdb_types::jsonb::gin_extract_terms`
/// - JSON/Text: uses `axiomdb_types::jsonb::gin_extract_terms_from_str`
/// - Array: uses `gin_extract_array_keys` (flattens all leaf elements)
pub(crate) fn gin_terms_for_row_value(
    value: Option<&Value>,
) -> Result<Option<Vec<Vec<u8>>>, DbError> {
    let terms = match value {
        Some(Value::Jsonb(b)) => axiomdb_types::jsonb::gin_extract_terms(b.as_slice())?,
        Some(Value::Json(s)) | Some(Value::Text(s)) => {
            axiomdb_types::jsonb::gin_extract_terms_from_str(s)?
        }
        Some(Value::Array(arr)) => gin_extract_array_keys(arr),
        _ => return Ok(None),
    };
    Ok(Some(terms))
}

pub(crate) fn gin_terms_if_indexed(
    idx: &IndexDef,
    row: &[Value],
    compiled_pred: Option<&Expr>,
) -> Result<Option<Vec<Vec<u8>>>, DbError> {
    if idx.columns.is_empty() {
        return Ok(None);
    }
    if let Some(pred) = compiled_pred {
        if !is_truthy(&eval(pred, row)?) {
            return Ok(None);
        }
    }
    gin_terms_for_row_value(row.get(idx.columns[0].col_idx as usize))
}

pub(crate) fn gin_heap_key(term: &[u8], rid: RecordId) -> Vec<u8> {
    let mut key = Vec::with_capacity(term.len() + 1 + 10);
    key.extend_from_slice(term);
    key.push(GIN_SEPARATOR);
    key.extend_from_slice(&rid.page_id.to_le_bytes());
    key.extend_from_slice(&rid.slot_id.to_le_bytes());
    key
}

pub(crate) fn gin_clustered_key(term: &[u8], pk_key: &[u8]) -> Vec<u8> {
    let mut key = Vec::with_capacity(term.len() + 1 + pk_key.len());
    key.extend_from_slice(term);
    key.push(GIN_SEPARATOR);
    key.extend_from_slice(pk_key);
    key
}

pub(crate) fn gin_term_bounds(term: &[u8]) -> (Vec<u8>, Vec<u8>) {
    let mut lo = Vec::with_capacity(term.len() + 1);
    lo.extend_from_slice(term);
    lo.push(GIN_SEPARATOR);

    let mut hi = lo.clone();
    hi.resize(MAX_KEY_LEN, 0xFF);
    (lo, hi)
}

pub(crate) fn gin_key_suffix<'a>(key: &'a [u8], term: &[u8]) -> Option<&'a [u8]> {
    let sep_pos = term.len();
    if key.len() <= sep_pos || !key.starts_with(term) || key[sep_pos] != GIN_SEPARATOR {
        return None;
    }
    Some(&key[sep_pos + 1..])
}

pub(crate) fn encode_clustered_pk_key_from_row(
    index_name: &str,
    primary_cols: &[u16],
    row: &[Value],
) -> Result<Vec<u8>, DbError> {
    let mut pk_values = Vec::with_capacity(primary_cols.len());
    for col_idx in primary_cols {
        let value = row
            .get(*col_idx as usize)
            .cloned()
            .ok_or_else(|| DbError::InvalidValue {
                reason: format!(
                    "clustered GIN index '{index_name}' requires primary-key column {col_idx} in row with len {}",
                    row.len()
                ),
            })?;
        if matches!(value, Value::Null) {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "clustered GIN index '{index_name}' cannot build a primary-key bookmark with NULL column {col_idx}"
                ),
            });
        }
        pk_values.push(value);
    }
    encode_index_key(&pk_values)
}

// ── indexes_for_table ─────────────────────────────────────────────────────────

/// Returns all `IndexDef`s for the given table (including primary indexes).
///
/// The caller can filter with `!idx.is_primary` to get only secondary indexes.
pub fn indexes_for_table(
    table_id: u32,
    storage: &dyn StorageEngine,
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
/// For **expression indexes** (Phase 21.8), `compiled_index_exprs[i][j]` holds
/// the pre-compiled expression for `indexes[i].columns[j]`, or `None` for regular
/// columns. Callers produce this via [`crate::partial_index::compile_index_exprs`]
/// once per statement. Pass `&[]` (empty vec) if no indexes have expressions.
///
/// Returns a list of `(index_id, new_root_page_id)` for indexes whose root
/// changed due to a B-Tree split.  The caller should persist these via
/// `CatalogWriter::update_index_root`.
pub fn insert_into_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    rid: RecordId,
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
    compiled_preds: &[Option<Expr>],
    compiled_index_exprs: &[Vec<Option<Expr>>],
    snap: TransactionSnapshot,
) -> Result<Vec<(u32, u64)>, DbError> {
    insert_into_indexes_with_undo(
        indexes,
        row,
        rid,
        storage,
        bloom,
        compiled_preds,
        compiled_index_exprs,
        snap,
        None,
        None,
    )
}

/// Like [`insert_into_indexes`] but optionally records `UndoIndexInsert` ops
/// in the transaction's undo log so ROLLBACK can remove the B-Tree entries.
#[expect(
    clippy::too_many_arguments,
    reason = "index maintenance needs row data, storage state, predicates, and optional txn hooks"
)]
pub fn insert_into_indexes_with_undo(
    indexes: &[IndexDef],
    row: &[Value],
    rid: RecordId,
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
    compiled_preds: &[Option<Expr>],
    compiled_index_exprs: &[Vec<Option<Expr>>],
    snap: TransactionSnapshot,
    txn: Option<&axiomdb_wal::TxnManager>,
    mut conn_txn: Option<&mut axiomdb_wal::ConnectionTxn>,
) -> Result<Vec<(u32, u64)>, DbError> {
    let mut updated_roots = Vec::new();

    for (i, idx) in indexes
        .iter()
        .enumerate()
        .filter(|(_, i)| !i.columns.is_empty())
    {
        // Phase 11.1b: BRIN indexes — update range summary, no B-Tree insert.
        if idx.index_type == 1 {
            let brin_col_idx = idx.columns[0].col_idx as usize;
            let val = row.get(brin_col_idx).cloned().unwrap_or(Value::Null);
            let is_null = matches!(val, Value::Null);
            let encoded = if is_null {
                0
            } else {
                match &val {
                    Value::Int(v) => *v as i64,
                    Value::BigInt(v) => *v,
                    Value::Real(v) => v.to_bits() as i64,
                    Value::Date(d) => *d as i64,
                    Value::Timestamp(t) => *t,
                    Value::Bool(b) => *b as i64,
                    _ => continue, // non-numeric — skip BRIN update
                }
            };
            let range_id = (rid.page_id / idx.pages_per_range as u64) as u32;
            let _ = axiomdb_storage::brin::update_range_summary(
                storage,
                idx.root_page_id,
                range_id,
                encoded,
                is_null,
            );
            continue;
        }

        // Phase 11.6: FTS inverted index — tokenize and insert term postings.
        if idx.index_type == 3 {
            let fts_col_idx = idx.columns[0].col_idx as usize;
            let text = match row.get(fts_col_idx) {
                Some(Value::Text(s)) | Some(Value::Json(s)) => s,
                _ => continue,
            };
            let root_pid = std::sync::atomic::AtomicU64::new(idx.root_page_id);
            let tokens = crate::tokenizer::tokenize(text);
            for tok in &tokens {
                let mut key = tok.term.as_bytes().to_vec();
                key.push(0x00);
                key.extend_from_slice(&rid.page_id.to_le_bytes());
                key.extend_from_slice(&rid.slot_id.to_le_bytes());
                key.extend_from_slice(&tok.position.to_le_bytes());
                let _ =
                    axiomdb_index::BTree::insert_in(storage, &root_pid, &key, rid, idx.fillfactor);
            }
            let new_root = root_pid.load(std::sync::atomic::Ordering::Acquire);
            if new_root != idx.root_page_id {
                updated_roots.push((idx.index_id, new_root));
            }
            continue;
        }

        // Phase 11.17: GIN inverted index — extract JSONB terms and insert each term+rid.
        if idx.index_type == 4 {
            let Some(terms) = gin_terms_if_indexed(
                idx,
                row,
                compiled_preds.get(i).and_then(|pred| pred.as_ref()),
            )?
            else {
                continue;
            };
            let root_pid = std::sync::atomic::AtomicU64::new(idx.root_page_id);
            for term in &terms {
                let key = gin_heap_key(term, rid);
                axiomdb_index::BTree::insert_in(storage, &root_pid, &key, rid, idx.fillfactor)?;
                if let (Some(tm), Some(ref mut ct)) = (txn, &mut conn_txn) {
                    tm.record_index_insert(
                        ct,
                        idx.index_id,
                        root_pid.load(std::sync::atomic::Ordering::Acquire),
                        key,
                    );
                }
            }
            let new_root = root_pid.load(std::sync::atomic::Ordering::Acquire);
            if new_root != idx.root_page_id {
                updated_roots.push((idx.index_id, new_root));
            }
            continue;
        }

        // Phase 11.4b: Trigram indexes — extract n-grams and insert into B-Tree.
        if idx.index_type == 2 {
            let trgm_col_idx = idx.columns[0].col_idx as usize;
            let text = match row.get(trgm_col_idx) {
                Some(Value::Text(s)) | Some(Value::Json(s)) => s,
                _ => continue,
            };
            let root_pid = std::sync::atomic::AtomicU64::new(idx.root_page_id);
            let trigrams = crate::trigram::extract_trigrams(text);
            for trgm in &trigrams {
                let mut key = trgm.to_vec();
                key.extend_from_slice(&encode_rid(rid));
                let _ =
                    axiomdb_index::BTree::insert_in(storage, &root_pid, &key, rid, idx.fillfactor);
            }
            let new_root = root_pid.load(std::sync::atomic::Ordering::Acquire);
            if new_root != idx.root_page_id {
                updated_roots.push((idx.index_id, new_root));
            }
            continue;
        }

        let original_root = idx.root_page_id;
        let mut current_root = original_root;

        // Partial index predicate check (Phase 6.7).
        if let Some(Some(pred)) = compiled_preds.get(i) {
            if !is_truthy(&eval(pred, row)?) {
                continue;
            }
        }

        // Phase 21.8: expression index key extraction.
        // Use index_key_values_if_indexed_with_exprs when compiled expressions
        // are available; fall back to plain column access for regular indexes.
        let idx_exprs = compiled_index_exprs.get(i);
        let key_vals: Vec<Value> = if let Some(exprs) = idx_exprs {
            match index_key_values_if_indexed_with_exprs(idx, row, None, exprs)? {
                Some(vals) => vals,
                None => continue, // NULL in key column → not indexed
            }
        } else {
            idx.columns
                .iter()
                .map(|c| row.get(c.col_idx as usize).cloned().unwrap_or(Value::Null))
                .collect()
        };

        // Skip NULL key values — NULLs are not indexed in secondary indexes.
        // This is consistent with SQL semantics (NULL ≠ NULL) and avoids
        // DuplicateKey errors from the B-Tree when multiple NULLs are inserted
        // into a UNIQUE index.
        // (Already filtered by index_key_values_if_indexed_with_exprs above,
        // but kept as safety net for the no-expr path.)
        if key_vals.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }
        let include_vals = index_include_values(idx, row);

        let key = encode_secondary_entry_key(idx, &key_vals, &include_vals, rid)?;

        // Uniqueness check — skip for FK auto-indexes (never unique by FK semantics).
        // Phase 7.3b: check heap visibility for existing entry — dead entries don't
        // count as duplicates (they'll be cleaned by vacuum).
        if idx.is_unique && !idx.is_fk_index {
            let logical_key = encode_index_key(&key_vals)?;
            let hi = logical_key_upper_bound(&logical_key);
            let existing = BTree::range_in(storage, current_root, Some(&logical_key), Some(&hi))?;
            if !existing.is_empty() {
                let root_pid = AtomicU64::new(current_root);
                for (existing_rid, existing_key) in existing {
                    if HeapChain::is_slot_visible(
                        storage,
                        existing_rid.page_id,
                        existing_rid.slot_id,
                        snap.clone(),
                    )? {
                        let dup_val = key_vals.first().map(|v| format!("{v}"));
                        return Err(DbError::UniqueViolation {
                            index_name: idx.name.clone(),
                            value: dup_val,
                        });
                    }
                    let _ = BTree::delete_in(storage, &root_pid, &existing_key);
                }
                current_root = root_pid.load(Ordering::Acquire);
            }
        }

        let root_pid = AtomicU64::new(current_root);
        BTree::insert_in(storage, &root_pid, &key, rid, idx.fillfactor)?;
        bloom.add(idx.index_id, &key);
        current_root = root_pid.load(Ordering::Acquire);
        if current_root != original_root {
            updated_roots.push((idx.index_id, current_root));
        }

        // Record undo op so ROLLBACK can remove this B-Tree entry.
        if let (Some(tm), Some(ref mut ct)) = (txn, &mut conn_txn) {
            tm.record_index_insert(ct, idx.index_id, current_root, key);
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
/// For **expression indexes** (Phase 21.8), `compiled_index_exprs[i][j]` holds
/// the pre-compiled expression for `indexes[i].columns[j]`, or `None` for regular
/// columns. Callers produce this via [`crate::partial_index::compile_index_exprs`]
/// once per statement.
///
/// Returns a list of `(index_id, new_root_page_id)` for indexes whose root
/// changed due to a collapse after deletion.
pub fn delete_from_indexes(
    indexes: &[IndexDef],
    row: &[Value],
    rid: RecordId,
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
    compiled_preds: &[Option<Expr>],
    compiled_index_exprs: &[Vec<Option<Expr>>],
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

        // Phase 11.17: GIN — delete all term entries for this row.
        if idx.index_type == 4 {
            let Some(terms) = gin_terms_for_row_value(row.get(idx.columns[0].col_idx as usize))?
            else {
                continue;
            };
            let root_pid = AtomicU64::new(idx.root_page_id);
            for term in &terms {
                let key = gin_heap_key(term, rid);
                let _ = BTree::delete_in(storage, &root_pid, &key)?;
            }
            bloom.mark_dirty(idx.index_id);
            let new_root = root_pid.load(Ordering::Acquire);
            if new_root != idx.root_page_id {
                updated_roots.push((idx.index_id, new_root));
            }
            continue;
        }

        // Phase 21.8: expression index key extraction.
        let idx_exprs = compiled_index_exprs.get(i);
        let key_vals: Vec<Value> = if let Some(exprs) = idx_exprs {
            match index_key_values_if_indexed_with_exprs(idx, row, None, exprs)? {
                Some(vals) => vals,
                None => continue, // NULL in key column → not indexed
            }
        } else {
            idx.columns
                .iter()
                .map(|c| row.get(c.col_idx as usize).cloned().unwrap_or(Value::Null))
                .collect()
        };

        // Skip NULL key values — NULLs were not inserted into the index.
        if key_vals.iter().any(|v| matches!(v, Value::Null)) {
            continue;
        }

        // FK auto-indexes and non-unique indexes: key || encode_rid(rid).
        // Unique indexes: plain encode_index_key.
        let include_vals = index_include_values(idx, row);
        let key = match encode_secondary_entry_key(idx, &key_vals, &include_vals, rid) {
            Ok(k) => k,
            Err(DbError::IndexKeyTooLong { .. }) => continue,
            Err(e) => return Err(e),
        };

        let root_pid = AtomicU64::new(idx.root_page_id);
        // Ignore NotFound (key may not exist if index was created after the row).
        let _ = BTree::delete_in(storage, &root_pid, &key)?;
        if !idx.include_columns.is_empty() {
            if let Ok(legacy_key) = encode_secondary_entry_key_legacy(idx, &key_vals, rid) {
                let _ = BTree::delete_in(storage, &root_pid, &legacy_key)?;
            }
        }
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
            let include_vals = index_include_values(idx, row);
            let key = match encode_secondary_entry_key(idx, &key_vals, &include_vals, *rid) {
                Ok(k) => k,
                Err(DbError::IndexKeyTooLong { .. }) => continue,
                Err(e) => return Err(e),
            };
            buckets[i].push(key);
            if !idx.include_columns.is_empty() {
                let legacy = match encode_secondary_entry_key_legacy(idx, &key_vals, *rid) {
                    Ok(k) => k,
                    Err(DbError::IndexKeyTooLong { .. }) => continue,
                    Err(e) => return Err(e),
                };
                buckets[i].push(legacy);
            }
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
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
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

pub(crate) fn index_include_values(idx: &IndexDef, row: &[Value]) -> Vec<Value> {
    idx.include_columns
        .iter()
        .map(|c| row.get(*c as usize).cloned().unwrap_or(Value::Null))
        .collect()
}

fn combine_encoded_index_bytes(
    key_vals: &[Value],
    include_vals: &[Value],
    rid_suffix: Option<RecordId>,
) -> Result<Vec<u8>, DbError> {
    let mut key = encode_index_key(key_vals)?;
    if !include_vals.is_empty() {
        key.extend_from_slice(&encode_index_key(include_vals)?);
    }
    if let Some(rid) = rid_suffix {
        key.extend_from_slice(&encode_rid(rid));
    }
    if key.len() > MAX_INDEX_KEY {
        return Err(DbError::IndexKeyTooLong {
            key_len: key.len(),
            max: MAX_INDEX_KEY,
        });
    }
    Ok(key)
}

pub(crate) fn encode_secondary_entry_key(
    idx: &IndexDef,
    key_vals: &[Value],
    include_vals: &[Value],
    rid: RecordId,
) -> Result<Vec<u8>, DbError> {
    combine_encoded_index_bytes(
        key_vals,
        include_vals,
        if idx.is_fk_index || !idx.is_unique {
            Some(rid)
        } else {
            None
        },
    )
}

pub(crate) fn encode_secondary_entry_key_legacy(
    idx: &IndexDef,
    key_vals: &[Value],
    rid: RecordId,
) -> Result<Vec<u8>, DbError> {
    combine_encoded_index_bytes(
        key_vals,
        &[],
        if idx.is_fk_index || !idx.is_unique {
            Some(rid)
        } else {
            None
        },
    )
}

pub(crate) fn logical_key_upper_bound(prefix: &[u8]) -> Vec<u8> {
    let mut v = prefix.to_vec();
    if v.len() < MAX_INDEX_KEY {
        v.resize(MAX_INDEX_KEY, 0xFF);
    } else {
        v.push(0xFF);
    }
    v
}

pub(crate) fn prefix_scan_secondary_entries(
    storage: &dyn StorageEngine,
    idx: &IndexDef,
    logical_key: &[u8],
) -> Result<Vec<(RecordId, Vec<u8>)>, DbError> {
    let hi = logical_key_upper_bound(logical_key);
    BTree::range_in(storage, idx.root_page_id, Some(logical_key), Some(&hi))
}

pub(crate) fn lookup_secondary_rids_by_logical_key(
    storage: &dyn StorageEngine,
    idx: &IndexDef,
    logical_key: &[u8],
) -> Result<Vec<RecordId>, DbError> {
    Ok(prefix_scan_secondary_entries(storage, idx, logical_key)?
        .into_iter()
        .map(|(rid, _)| rid)
        .collect())
}

pub(crate) fn decode_secondary_entry_values(
    idx: &IndexDef,
    key: &[u8],
) -> Result<(Vec<Value>, Vec<Value>), DbError> {
    let (key_vals, consumed) = decode_index_key(key, idx.columns.len())?;
    if idx.include_columns.is_empty() {
        return Ok((key_vals, vec![]));
    }
    let (include_vals, _) = decode_index_key(&key[consumed..], idx.include_columns.len())?;
    Ok((key_vals, include_vals))
}

/// Extracts the key value for a single index column from a row.
///
/// For expression columns: evaluates the compiled expression.
/// For regular columns: reads `row[col.col_idx]` directly.
///
/// Returns `Ok(None)` if the result is `Value::Null` (NULL is not indexed).
pub fn index_key_value_for_column(
    col: &IndexColumnDef,
    compiled_expr: Option<&Expr>,
    row: &[Value],
) -> Result<Option<Value>, DbError> {
    let value = match (&col.expr, compiled_expr) {
        (Some(_), Some(expr)) => eval(expr, row)?,
        (None, None) => row
            .get(col.col_idx as usize)
            .cloned()
            .unwrap_or(Value::Null),
        _ => {
            return Err(DbError::Internal {
                message: format!(
                    "index_key_value_for_column: expr mismatch for column {} (expr={:?}, compiled={:?})",
                    col.col_idx, col.expr.as_ref().map(|_| "Some"), compiled_expr.map(|_| "Some")
                ),
            });
        }
    };
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    Ok(Some(value))
}

/// Like [`index_key_values_if_indexed`] but accepts per-column compiled expressions
/// for expression indexes (Phase 21.8).
///
/// `compiled_exprs` is parallel to `idx.columns` — `compiled_exprs[i]` is the
/// resolved `Expr` for `idx.columns[i]`, or `None` for regular columns.
pub fn index_key_values_if_indexed_with_exprs(
    idx: &IndexDef,
    row: &[Value],
    compiled_pred: Option<&Expr>,
    compiled_exprs: &[Option<Expr>],
) -> Result<Option<Vec<Value>>, DbError> {
    if let Some(pred) = compiled_pred {
        if !is_truthy(&eval(pred, row)?) {
            return Ok(None);
        }
    }

    let mut key_vals = Vec::with_capacity(idx.columns.len());
    for (i, col) in idx.columns.iter().enumerate() {
        let compiled_expr = compiled_exprs.get(i).and_then(|e| e.as_ref());
        let value = index_key_value_for_column(col, compiled_expr, row)?;
        match value {
            Some(v) => key_vals.push(v),
            None => return Ok(None), // NULL in any key column → not indexed
        }
    }
    Ok(Some(key_vals))
}

/// Returns `true` if updating `(old_row, old_rid)` to `(new_row, new_rid)` requires
/// maintenance for `idx`.
///
/// If the RID changes, the index is always affected. When the RID is stable, the
/// index is affected only if its membership or logical key changes.
///
/// For **expression indexes** (Phase 21.8), `compiled_index_exprs` provides
/// per-column compiled expressions aligned with `idx.columns`. When provided,
/// expression columns are evaluated against both old_row and new_row to detect key changes.
pub fn update_affects_index(
    idx: &IndexDef,
    compiled_pred: Option<&Expr>,
    old_row: &[Value],
    old_rid: RecordId,
    new_row: &[Value],
    new_rid: RecordId,
    compiled_index_exprs: Option<&[Option<Expr>]>,
) -> Result<bool, DbError> {
    if old_rid != new_rid {
        return Ok(true);
    }
    let include_changed = idx.include_columns.iter().any(|col_idx| {
        old_row.get(*col_idx as usize).unwrap_or(&Value::Null)
            != new_row.get(*col_idx as usize).unwrap_or(&Value::Null)
    });

    // Phase 21.8: expression-aware key comparison.
    // When compiled expressions are available, use index_key_values_if_indexed_with_exprs.
    if let Some(exprs) = compiled_index_exprs {
        let old_key_vals =
            index_key_values_if_indexed_with_exprs(idx, old_row, compiled_pred, exprs)?;
        let new_key_vals =
            index_key_values_if_indexed_with_exprs(idx, new_row, compiled_pred, exprs)?;
        return Ok(match (old_key_vals, new_key_vals) {
            (None, None) => false,
            (Some(old_vals), Some(new_vals)) => old_vals != new_vals || include_changed,
            _ => true,
        });
    }

    // Fallback: regular column access (backwards compatible).
    let old_key_vals = index_key_values_if_indexed(idx, old_row, compiled_pred)?;
    let new_key_vals = index_key_values_if_indexed(idx, new_row, compiled_pred)?;
    Ok(match (old_key_vals, new_key_vals) {
        (None, None) => false,
        (Some(old_vals), Some(new_vals)) => old_vals != new_vals || include_changed,
        _ => true,
    })
}

/// Removes all `keys` from a single index with one `delete_many_in` call.
pub fn delete_many_from_single_index(
    idx: &mut IndexDef,
    keys: &[Vec<u8>],
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
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
/// For **expression indexes** (Phase 21.8), `compiled_index_exprs[i][j]` holds
/// the pre-compiled expression for `indexes[i].columns[j]`, or `None` for regular
/// columns. Pass `&[]` (empty vec) if no indexes have expressions.
///
/// Returns `(index_id, new_root_page_id)` for every index whose root changed.
/// The caller is responsible for updating the in-memory `IndexDef` slice.
#[allow(clippy::too_many_arguments)]
pub fn batch_insert_into_indexes(
    indexes: &mut [IndexDef],
    rows: &[Vec<Value>],
    rids: &[RecordId],
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
    compiled_preds: &[Option<Expr>],
    compiled_index_exprs: &[Vec<Option<Expr>>],
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
        let idx_exprs = compiled_index_exprs.get(i);

        if idx.index_type == 4 {
            let original_root = idx.root_page_id;
            let root_pid = AtomicU64::new(original_root);
            for (row, rid) in rows.iter().zip(rids.iter()) {
                let Some(terms) = gin_terms_if_indexed(idx, row, pred)? else {
                    continue;
                };
                for term in &terms {
                    let key = gin_heap_key(term, *rid);
                    BTree::insert_in(storage, &root_pid, &key, *rid, idx.fillfactor)?;
                }
            }
            let new_root = root_pid.load(Ordering::Acquire);
            if new_root != original_root {
                idx.root_page_id = new_root;
                updated_roots.push((idx.index_id, new_root));
            }
            continue;
        }

        // ── Collect (encoded_key, rid) for this index ────────────────────────
        let mut pairs: Vec<(Vec<u8>, RecordId)> = Vec::new();
        for (row, rid) in rows.iter().zip(rids.iter()) {
            if let Some(p) = pred {
                if !is_truthy(&eval(p, row)?) {
                    continue;
                }
            }
            // Phase 21.8: expression index key extraction.
            let key_vals: Vec<Value> = if let Some(exprs) = idx_exprs {
                match index_key_values_if_indexed_with_exprs(idx, row, None, exprs)? {
                    Some(vals) => vals,
                    None => continue, // NULL in key column → not indexed
                }
            } else {
                idx.columns
                    .iter()
                    .map(|c| row.get(c.col_idx as usize).cloned().unwrap_or(Value::Null))
                    .collect()
            };
            let include_vals = index_include_values(idx, row);
            let key = encode_secondary_entry_key(idx, &key_vals, &include_vals, *rid)?;
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
            if !skip_unique_check && idx.is_unique && !idx.is_fk_index {
                let cur_root = root_pid.load(Ordering::Acquire);
                let (logical_vals, _) = decode_index_key(key, idx.columns.len())?;
                let logical_key = encode_index_key(&logical_vals)?;
                let hi = logical_key_upper_bound(&logical_key);
                let existing = BTree::range_in(storage, cur_root, Some(&logical_key), Some(&hi))?;
                if !existing.is_empty() {
                    let del_pid = AtomicU64::new(cur_root);
                    for (existing_rid, existing_key) in existing {
                        if HeapChain::is_slot_visible(
                            storage,
                            existing_rid.page_id,
                            existing_rid.slot_id,
                            snap.clone(),
                        )? {
                            let dup_val = Some("(encoded)".to_string());
                            return Err(DbError::UniqueViolation {
                                index_name: idx.name.clone(),
                                value: dup_val,
                            });
                        }
                        let _ = BTree::delete_in(storage, &del_pid, &existing_key);
                    }
                    let del_root = del_pid.load(Ordering::Acquire);
                    if del_root != cur_root {
                        root_pid.store(del_root, Ordering::Release);
                    }
                }
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
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
    snap: TransactionSnapshot,
) -> Result<Option<u64>, DbError> {
    if idx.columns.is_empty() || rows.is_empty() {
        return Ok(None);
    }

    if idx.index_type == 4 {
        let original_root = idx.root_page_id;
        let root_pid = AtomicU64::new(original_root);

        for (row, rid) in rows {
            let Some(terms) = gin_terms_if_indexed(idx, row, compiled_pred)? else {
                continue;
            };
            for term in &terms {
                let key = gin_heap_key(term, *rid);
                BTree::insert_in(storage, &root_pid, &key, *rid, idx.fillfactor)?;
            }
        }

        let new_root = root_pid.load(Ordering::Acquire);
        if new_root != original_root {
            idx.root_page_id = new_root;
            Ok(Some(new_root))
        } else {
            Ok(None)
        }
    } else {
        let original_root = idx.root_page_id;
        let root_pid = AtomicU64::new(original_root);

        for (row, rid) in rows {
            let Some(key_vals) = index_key_values_if_indexed(idx, row, compiled_pred)? else {
                continue;
            };
            let include_vals = index_include_values(idx, row);
            let key = encode_secondary_entry_key(idx, &key_vals, &include_vals, *rid)?;

            if idx.is_unique && !idx.is_fk_index {
                let cur_root = root_pid.load(Ordering::Acquire);
                let logical_key = encode_index_key(&key_vals)?;
                let hi = logical_key_upper_bound(&logical_key);
                let existing = BTree::range_in(storage, cur_root, Some(&logical_key), Some(&hi))?;
                if !existing.is_empty() {
                    let del_pid = AtomicU64::new(cur_root);
                    for (existing_rid, existing_key) in existing {
                        if HeapChain::is_slot_visible(
                            storage,
                            existing_rid.page_id,
                            existing_rid.slot_id,
                            snap.clone(),
                        )? {
                            let dup_val = key_vals.first().map(|v| format!("{v}"));
                            return Err(DbError::UniqueViolation {
                                index_name: idx.name.clone(),
                                value: dup_val,
                            });
                        }
                        let _ = BTree::delete_in(storage, &del_pid, &existing_key);
                    }
                    let del_root = del_pid.load(Ordering::Acquire);
                    if del_root != cur_root {
                        root_pid.store(del_root, Ordering::Release);
                    }
                }
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
}

/// Inserts one row into a single index and returns the new root if it changed.
pub fn insert_into_single_index(
    idx: &mut IndexDef,
    compiled_pred: Option<&Expr>,
    row: &[Value],
    rid: RecordId,
    storage: &dyn StorageEngine,
    bloom: &crate::bloom::BloomRegistry,
    snap: TransactionSnapshot,
) -> Result<Option<u64>, DbError> {
    if idx.columns.is_empty() {
        return Ok(None);
    }

    if idx.index_type == 4 {
        let Some(terms) = gin_terms_if_indexed(idx, row, compiled_pred)? else {
            return Ok(None);
        };

        let root_pid = AtomicU64::new(idx.root_page_id);
        for term in &terms {
            let key = gin_heap_key(term, rid);
            BTree::insert_in(storage, &root_pid, &key, rid, idx.fillfactor)?;
        }
        let new_root = root_pid.load(Ordering::Acquire);
        if new_root != idx.root_page_id {
            idx.root_page_id = new_root;
            return Ok(Some(new_root));
        }
        return Ok(None);
    }

    let Some(key_vals) = index_key_values_if_indexed(idx, row, compiled_pred)? else {
        return Ok(None);
    };
    let include_vals = index_include_values(idx, row);
    let key = encode_secondary_entry_key(idx, &key_vals, &include_vals, rid)?;

    if idx.is_unique && !idx.is_fk_index {
        let logical_key = encode_index_key(&key_vals)?;
        let hi = logical_key_upper_bound(&logical_key);
        let existing = BTree::range_in(storage, idx.root_page_id, Some(&logical_key), Some(&hi))?;
        if !existing.is_empty() {
            let del_pid = AtomicU64::new(idx.root_page_id);
            for (existing_rid, existing_key) in existing {
                if HeapChain::is_slot_visible(
                    storage,
                    existing_rid.page_id,
                    existing_rid.slot_id,
                    snap.clone(),
                )? {
                    let dup_val = key_vals.first().map(|v| format!("{v}"));
                    return Err(DbError::UniqueViolation {
                        index_name: idx.name.clone(),
                        value: dup_val,
                    });
                }
                let _ = BTree::delete_in(storage, &del_pid, &existing_key);
            }
            let del_root = del_pid.load(Ordering::Acquire);
            if del_root != idx.root_page_id {
                idx.root_page_id = del_root;
            }
        }
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
                expr: None,
            }],
            predicate: None,
            fillfactor: 90,
            is_fk_index: false,
            include_columns: vec![],
            index_type: 0,
            pages_per_range: 128,
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
            !update_affects_index(&idx, None, &old_row, old_rid, &new_row, new_rid, None).unwrap(),
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
                None,
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
            update_affects_index(&idx, Some(&predicate), &old_row, rid, &new_row, rid, None,)
                .unwrap(),
            "partial index membership changes must force maintenance even with stable RID"
        );
    }
}
