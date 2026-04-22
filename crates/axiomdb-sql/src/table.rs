//! Table engine — row storage interface for user tables.
//!
//! [`TableEngine`] bridges the SQL executor (which operates on [`Value`] rows)
//! and the raw storage layer (which operates on `&[u8]` bytes in heap pages).
//!
//! ## Responsibilities
//!
//! - **Scan:** iterate all MVCC-visible rows, decoding bytes to `Vec<Value>`.
//! - **Insert:** coerce + encode values, write to `HeapChain`, WAL-log.
//! - **Delete:** read old bytes, stamp deletion in `HeapChain`, WAL-log.
//! - **Update:** delete old row + insert new row (two WAL entries).
//!
//! ## Usage
//!
//! All methods are stateless — the caller provides `storage` and `txn` on each
//! call. The executor (Phase 4.5) constructs a `TableEngine` and passes them
//! through for the lifetime of the statement.
//!
//! ```rust,ignore
//! // Resolve table from catalog first:
//! let resolved = resolver.resolve_table(None, "users")?;
//!
//! // Scan:
//! let rows = TableEngine::scan_table(storage, &resolved.def, &resolved.columns, snap)?;
//!
//! // Insert (requires active transaction):
//! txn.begin()?;
//! let rid = TableEngine::insert_row(
//!     storage, txn, &resolved.def, &resolved.columns,
//!     vec![Value::BigInt(1), Value::Text("alice".into())],
//! )?;
//! txn.commit()?;
//! ```
//!
//! ## WAL key convention
//!
//! Since Phase 4.5b does not enforce primary key constraints, the WAL `key` for
//! every user-table DML entry is the physical location of the row encoded as
//! 10 bytes: `[page_id: 8 LE][slot_id: 2 LE]`. This is supplemented by the
//! physical location already embedded in the WAL value bytes by `TxnManager`.
//!
//! ## UPDATE semantics
//!
//! `update_row` is implemented as `delete_row` + `insert_row` (two separate WAL
//! entries). `TxnManager::record_update` is not used because it assumes old and
//! new slots are on the same page, which is not guaranteed when the old page is
//! full and the chain must grow.

use axiomdb_catalog::schema::{ColumnDef, ColumnType, IndexDef, TableDef};
use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot};
use axiomdb_index::BTree;
use axiomdb_storage::{
    heap_chain, num_slots, read_slot, HeapAppendHint, HeapChain, Page, RowHeader, StorageEngine,
};
use axiomdb_types::{
    codec::{decode_row, decode_row_masked, encode_row},
    coerce::{coerce, CoercionMode},
    DataType, Value,
};
use axiomdb_wal::TxnManager;
use std::mem::size_of;

use crate::{clustered_secondary::ClusteredSecondaryLayout, key_encoding::encode_index_key};

use crate::session::SessionContext;

type StableUpdateBatchRef<'a> = (&'a [u8], &'a [u8], &'a [u8], u64, u16);

fn ensure_heap_table(table_def: &TableDef, feature: &str) -> Result<(), DbError> {
    table_def.ensure_heap_runtime(feature)
}

// ── TableEngine ───────────────────────────────────────────────────────────────

/// Stateless row storage interface for user tables.
///
/// Follows the same unit-struct pattern as [`HeapChain`]: all methods take
/// storage and transaction state as explicit parameters.
pub struct TableEngine;

include!("table_scan.rs");
include!("table_write.rs");
include!("table_ctx.rs");

/// Re-reads a single heap row by `RecordId` and returns decoded values if
/// MVCC-visible. Returns `None` if the slot is empty or invisible.
///
/// Used by the lock re-verification path (Phase 40.11): after waiting for a
/// row X-lock, the row may have been modified or deleted by the transaction
/// that previously held the lock. This function re-reads the latest version.
pub fn reread_heap_row_if_visible(
    storage: &dyn StorageEngine,
    rid: axiomdb_core::RecordId,
    snap: &axiomdb_core::TransactionSnapshot,
    col_types: &[DataType],
) -> Option<Vec<Value>> {
    let raw = storage.read_page(rid.page_id).ok()?;
    let page = axiomdb_storage::Page::from_bytes(*raw.as_bytes()).ok()?;
    let (header, data) = axiomdb_storage::heap::read_tuple(&page, rid.slot_id).ok()??;
    if !header.is_visible(snap) {
        return None;
    }
    axiomdb_types::codec::decode_row(data, col_types).ok()
}

// ── TOAST de-externalization (Phase 11.2) ─────────────────────────────────────

const TOAST_PREFIX: &str = "__toast__:";

/// Resolves TOAST placeholder values in a decoded row by reading overflow chains.
///
/// The codec produces `Value::Text("__toast__:page_id:compressed:raw_len")` for
/// externalized values. This function detects those placeholders, reads the
/// overflow chain via `clustered_overflow::read_blob_chain`, optionally
/// LZ4-decompresses, and replaces
/// the placeholder with the real value.
///
/// For rows without TOAST placeholders, this is a no-op (fast check per value).
pub fn detoast_row(row: &mut [Value], storage: &dyn StorageEngine) {
    for val in row.iter_mut() {
        match val {
            Value::Text(s) if s.starts_with(TOAST_PREFIX) => {
                if let Some(resolved) = resolve_toast_text(s, storage) {
                    *val = Value::Text(resolved);
                }
            }
            Value::Json(s) if s.starts_with(TOAST_PREFIX) => {
                if let Some(resolved) = resolve_toast_text(s, storage) {
                    *val = Value::Json(resolved);
                }
            }
            Value::Bytes(b) => {
                if let Ok(s) = std::str::from_utf8(b) {
                    if s.starts_with(TOAST_PREFIX) {
                        if let Some(resolved) = resolve_toast_bytes(s, storage) {
                            *val = Value::Bytes(resolved);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

/// Parses a TOAST placeholder string and reads the overflow chain.
fn resolve_toast_text(placeholder: &str, storage: &dyn StorageEngine) -> Option<String> {
    let (page_id, compressed, raw_len) = parse_toast_placeholder(placeholder)?;
    let data =
        axiomdb_storage::clustered_overflow::read_blob_chain(storage, page_id, raw_len).ok()?;

    let bytes = if compressed {
        lz4_flex::decompress_size_prepended(&data).ok()?
    } else {
        data
    };

    String::from_utf8(bytes).ok()
}

fn resolve_toast_bytes(placeholder: &str, storage: &dyn StorageEngine) -> Option<Vec<u8>> {
    let (page_id, compressed, raw_len) = parse_toast_placeholder(placeholder)?;
    let data =
        axiomdb_storage::clustered_overflow::read_blob_chain(storage, page_id, raw_len).ok()?;

    if compressed {
        lz4_flex::decompress_size_prepended(&data).ok()
    } else {
        Some(data)
    }
}

fn parse_toast_placeholder(placeholder: &str) -> Option<(u64, bool, usize)> {
    let rest = placeholder.strip_prefix(TOAST_PREFIX)?;
    let mut parts = rest.split(':');
    let page_id: u64 = parts.next()?.parse().ok()?;
    let compressed: bool = parts.next()?.parse().ok()?;
    let raw_len: usize = parts.next()?.parse().ok()?;
    Some((page_id, compressed, raw_len))
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Extracts `DataType` from each `ColumnDef` in declaration order.
///
/// Maps a single `ColumnType` → `DataType`.
pub fn column_type_to_data_type(ct: ColumnType) -> DataType {
    match ct {
        ColumnType::Bool => DataType::Bool,
        ColumnType::Int => DataType::Int,
        ColumnType::BigInt => DataType::BigInt,
        ColumnType::Float => DataType::Real,
        ColumnType::Decimal => DataType::Decimal,
        ColumnType::Text => DataType::Text,
        ColumnType::Json => DataType::Json,
        ColumnType::Jsonb => DataType::Jsonb,
        ColumnType::Bytes => DataType::Bytes,
        ColumnType::Date => DataType::Date,
        ColumnType::Timestamp => DataType::Timestamp,
        ColumnType::Uuid => DataType::Uuid,
    }
}

/// `ColumnType` (compact catalog representation) maps to `DataType`
/// (full in-memory type used by the row codec and expression evaluator).
pub fn column_data_types(columns: &[ColumnDef]) -> Vec<DataType> {
    columns
        .iter()
        .map(|c| match c.col_type {
            ColumnType::Bool => DataType::Bool,
            ColumnType::Int => DataType::Int,
            ColumnType::BigInt => DataType::BigInt,
            ColumnType::Float => DataType::Real,
            ColumnType::Decimal => DataType::Decimal,
            ColumnType::Text => DataType::Text,
            ColumnType::Json => DataType::Json,
            ColumnType::Bytes => DataType::Bytes,
            ColumnType::Date => DataType::Date,
            ColumnType::Timestamp => DataType::Timestamp,
            ColumnType::Uuid => DataType::Uuid,
            ColumnType::Jsonb => DataType::Jsonb,
        })
        .collect()
}

/// Decodes a raw row byte slice into a `Vec<Value>` using column definitions.
///
/// Public helper for modules (e.g., `fk_enforcement`) that read rows from the
/// heap and need to decode them without going through `scan_table`.
pub fn decode_row_from_bytes(bytes: &[u8], columns: &[ColumnDef]) -> Result<Vec<Value>, DbError> {
    let col_types = column_data_types(columns);
    decode_row(bytes, &col_types)
}

// ── Clustered table scan helpers (Phase 39.15) ──────────────────────────────

/// COUNT(*) fast path for clustered tables: walks the leaf chain and counts
/// cells with visible RowHeaders WITHOUT decoding any row data.
/// Only reads the 24-byte RowHeader per cell (~20× faster than full decode).
pub fn count_clustered_visible(
    storage: &dyn StorageEngine,
    root_pid: u64,
    snap: TransactionSnapshot,
) -> Result<u64, DbError> {
    use axiomdb_storage::{clustered_internal, clustered_leaf, page::PageType};

    // Descend to leftmost leaf.
    let mut pid = root_pid;
    loop {
        let page = storage.read_page(pid)?;
        let pt = PageType::try_from(page.header().page_type)
            .map_err(|e| DbError::Other(format!("{e}")))?;
        match pt {
            PageType::ClusteredLeaf => break,
            PageType::ClusteredInternal => {
                pid = clustered_internal::child_at(&page, 0)?;
            }
            _ => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!("count_clustered_visible: unexpected page type {pt:?} at {pid}"),
                });
            }
        }
    }

    // Walk leaf chain, count visible cells.
    let mut count = 0u64;
    let mut current = pid;
    while current != clustered_leaf::NULL_PAGE {
        let page = storage.read_page(current)?;
        let n = clustered_leaf::num_cells(&page);
        for idx in 0..n {
            let cell = clustered_leaf::read_cell(&page, idx)?;
            if cell.row_header.is_visible(&snap) {
                count += 1;
            }
        }
        current = clustered_leaf::next_leaf(&page);
    }

    Ok(count)
}

/// Dummy RecordId for clustered rows (no heap slot).  The downstream pipeline
/// (WHERE recheck, ORDER BY, GROUP BY, projection) only uses the `Vec<Value>`;
/// the RecordId is carried for type compatibility and is never used as a
/// heap address for clustered tables.
const CLUSTERED_DUMMY_RID: RecordId = RecordId {
    page_id: 0,
    slot_id: 0,
};

/// Full-table scan of a clustered B-tree, returning visible rows decoded as
/// `Vec<Value>`.  Equivalent to `scan_table` for heap tables.
pub fn scan_clustered_table(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    scan_clustered_table_masked(storage, table_def, columns, snap, None)
}

/// Like [`scan_clustered_table`] but only decodes columns indicated by `mask`.
///
/// When `mask` is `Some`, columns with `mask[i] == false` are decoded as
/// `Value::Null`, saving heap allocations for TEXT/Bytes columns that the
/// query does not reference (e.g. aggregate queries on a subset of columns).
pub fn scan_clustered_table_masked(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    snap: TransactionSnapshot,
    mask: Option<&[bool]>,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    let col_types = column_data_types(columns);
    let mut result = Vec::new();
    axiomdb_storage::clustered_tree::scan_all_callback(
        storage,
        Some(table_def.root_page_id),
        &snap,
        |inline_data, overflow| {
            let values = if let Some((first_page, tail_len)) = overflow {
                // Overflow row: assemble full bytes first (rare for typical row sizes).
                let tail =
                    axiomdb_storage::clustered_overflow::read_chain(storage, first_page, tail_len)?;
                let mut full = Vec::with_capacity(inline_data.len() + tail.len());
                full.extend_from_slice(inline_data);
                full.extend_from_slice(&tail);
                match mask {
                    Some(m) => decode_row_masked(&full, &col_types, m)?,
                    None => decode_row(&full, &col_types)?,
                }
            } else {
                // Inline row (common case): decode directly — zero extra allocation.
                match mask {
                    Some(m) => decode_row_masked(inline_data, &col_types, m)?,
                    None => decode_row(inline_data, &col_types)?,
                }
            };
            result.push((CLUSTERED_DUMMY_RID, values));
            Ok(())
        },
    )?;
    Ok(result)
}

/// Point lookup on a clustered B-tree by primary key bytes.
pub fn lookup_clustered_row(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    pk_key: &[u8],
    snap: TransactionSnapshot,
) -> Result<Option<(RecordId, Vec<Value>)>, DbError> {
    let col_types = column_data_types(columns);
    match axiomdb_storage::clustered_tree::lookup(
        storage,
        Some(table_def.root_page_id),
        pk_key,
        &snap,
    )? {
        Some(row) => {
            let values = decode_row(&row.row_data, &col_types)?;
            Ok(Some((CLUSTERED_DUMMY_RID, values)))
        }
        None => Ok(None),
    }
}

/// Range scan on a clustered B-tree by primary key bounds.
pub fn range_clustered_table(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    use std::ops::Bound;
    let col_types = column_data_types(columns);
    let from = match lo {
        Some(k) => Bound::Included(k.to_vec()),
        None => Bound::Unbounded,
    };
    let to = match hi {
        Some(k) => Bound::Included(k.to_vec()),
        None => Bound::Unbounded,
    };
    let iter = axiomdb_storage::clustered_tree::range(
        storage,
        Some(table_def.root_page_id),
        from,
        to,
        &snap,
    )?;
    let mut result = Vec::new();
    for row_result in iter {
        let row = row_result?;
        let values = decode_row(&row.row_data, &col_types)?;
        result.push((CLUSTERED_DUMMY_RID, values));
    }
    Ok(result)
}

/// Scans a table using the appropriate storage layout (heap or clustered).
/// Unified entry point for JOIN paths and other multi-table contexts.
pub fn scan_table_any_layout(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    if table_def.is_clustered() {
        scan_clustered_table(storage, table_def, columns, snap)
    } else {
        TableEngine::scan_table(storage, table_def, columns, snap, None)
    }
}

/// Point lookup through a clustered secondary index:
/// `secondary key prefix -> PK bookmark -> clustered PK lookup`.
pub fn lookup_clustered_secondary_rows(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    primary_idx: &IndexDef,
    secondary_idx: &IndexDef,
    logical_prefix_key: &[u8],
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    let hi = clustered_secondary_high_bound(logical_prefix_key);
    let pairs = BTree::range_in(
        storage,
        secondary_idx.root_page_id,
        Some(logical_prefix_key),
        Some(&hi),
    )?;
    decode_clustered_secondary_pairs(
        storage,
        table_def,
        columns,
        primary_idx,
        secondary_idx,
        pairs,
        snap,
    )
}

/// Range scan through a clustered secondary index:
/// `secondary range -> PK bookmark(s) -> clustered PK lookup(s)`.
pub fn range_clustered_secondary_rows(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    primary_idx: &IndexDef,
    secondary_idx: &IndexDef,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    let hi_owned = hi.map(clustered_secondary_high_bound);
    let pairs = BTree::range_in(storage, secondary_idx.root_page_id, lo, hi_owned.as_deref())?;
    decode_clustered_secondary_pairs(
        storage,
        table_def,
        columns,
        primary_idx,
        secondary_idx,
        pairs,
        snap,
    )
}

fn decode_clustered_secondary_pairs(
    storage: &dyn StorageEngine,
    table_def: &TableDef,
    columns: &[ColumnDef],
    primary_idx: &IndexDef,
    secondary_idx: &IndexDef,
    pairs: Vec<(RecordId, Vec<u8>)>,
    snap: TransactionSnapshot,
) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
    let layout = ClusteredSecondaryLayout::derive(secondary_idx, primary_idx)?;
    let col_types = column_data_types(columns);
    let mut result = Vec::with_capacity(pairs.len());

    for (_rid, key_bytes) in pairs {
        let entry = layout.decode_entry_key(&key_bytes)?;
        let pk_key = encode_index_key(&entry.primary_key)?;
        let Some(row) = axiomdb_storage::clustered_tree::lookup(
            storage,
            Some(table_def.root_page_id),
            &pk_key,
            &snap,
        )?
        else {
            continue;
        };
        let values = decode_row(&row.row_data, &col_types)?;
        result.push((CLUSTERED_DUMMY_RID, values));
    }

    Ok(result)
}

fn clustered_secondary_high_bound(logical_key: &[u8]) -> Vec<u8> {
    let mut hi = logical_key.to_vec();
    if hi.len() < crate::key_encoding::MAX_INDEX_KEY {
        hi.resize(crate::key_encoding::MAX_INDEX_KEY, 0xFF);
    }
    hi
}

/// Encodes a `RecordId` as a 10-byte WAL key: `[page_id:8 LE][slot_id:2 LE]`.
fn encode_rid(page_id: u64, slot_id: u16) -> [u8; 10] {
    let mut buf = [0u8; 10];
    buf[..8].copy_from_slice(&page_id.to_le_bytes());
    buf[8..].copy_from_slice(&slot_id.to_le_bytes());
    buf
}

fn build_in_place_tuple_image(
    old_tuple_image: &[u8],
    new_encoded: &[u8],
    txn_id: u64,
) -> Result<Vec<u8>, DbError> {
    let header_len = size_of::<RowHeader>();
    if old_tuple_image.len() < header_len {
        return Err(DbError::Internal {
            message: "stable-RID update: old tuple image shorter than RowHeader".into(),
        });
    }
    let old_header: RowHeader = *bytemuck::from_bytes(&old_tuple_image[..header_len]);
    let new_header = RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version: old_header.row_version.saturating_add(1),
        _flags: old_header._flags,
    };
    let mut image = Vec::with_capacity(header_len + new_encoded.len());
    image.extend_from_slice(&new_header.txn_id_created.to_le_bytes());
    image.extend_from_slice(&new_header.txn_id_deleted.to_le_bytes());
    image.extend_from_slice(&new_header.row_version.to_le_bytes());
    image.extend_from_slice(&new_header._flags.to_le_bytes());
    image.extend_from_slice(new_encoded);
    Ok(image)
}

fn update_encoded_row_with_hint(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &TableDef,
    record_id: RecordId,
    new_encoded: &[u8],
    hint: Option<&mut HeapAppendHint>,
) -> Result<RecordId, DbError> {
    let old_bytes = HeapChain::read_row(storage, record_id.page_id, record_id.slot_id)?.ok_or(
        DbError::AlreadyDeleted {
            page_id: record_id.page_id,
            slot_id: record_id.slot_id,
        },
    )?;

    let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
    HeapChain::delete(storage, record_id.page_id, record_id.slot_id, txn_id)?;
    let old_key = encode_rid(record_id.page_id, record_id.slot_id);
    txn.record_delete(
        conn_txn,
        table_def.id,
        &old_key,
        &old_bytes,
        record_id.page_id,
        record_id.slot_id,
    )?;

    let (new_page_id, new_slot_id) = {
        let batch = &mut conn_txn.local_page_batch;
        match hint {
            Some(h) => HeapChain::insert_with_hint(
                storage,
                table_def.root_page_id,
                new_encoded,
                txn_id,
                Some(h),
                Some(batch),
            )?,
            None => HeapChain::insert(
                storage,
                table_def.root_page_id,
                new_encoded,
                txn_id,
                Some(batch),
            )?,
        }
    };
    let new_key = encode_rid(new_page_id, new_slot_id);
    txn.record_insert(
        conn_txn,
        table_def.id,
        &new_key,
        new_encoded,
        new_page_id,
        new_slot_id,
    )?;

    Ok(RecordId {
        page_id: new_page_id,
        slot_id: new_slot_id,
    })
}

fn apply_prepared_updates_preserve_rid(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_def: &TableDef,
    prepared: Vec<(RecordId, Vec<u8>)>,
    mut hint: Option<&mut HeapAppendHint>,
) -> Result<Vec<RecordId>, DbError> {
    if prepared.is_empty() {
        return Ok(Vec::new());
    }

    let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
    let rewrite_results =
        HeapChain::rewrite_batch_same_slot(storage, table_def.root_page_id, &prepared, txn_id)?;

    // Separate stable-RID successes from fallback rows.
    // Stable-RID rows are WAL-logged in one batch call; fallback rows use
    // the per-row delete+insert path.
    struct StableImage {
        key: [u8; 10],
        old_tuple_image: Vec<u8>,
        new_tuple_image: Vec<u8>,
        page_id: u64,
        slot_id: u16,
    }

    let mut new_rids = Vec::with_capacity(prepared.len());
    let mut stable_images: Vec<StableImage> = Vec::new();

    for ((rid, new_encoded), rewrite_result) in prepared.iter().zip(rewrite_results) {
        match rewrite_result {
            Some(old_tuple_image) => {
                let key = encode_rid(rid.page_id, rid.slot_id);
                let new_tuple_image =
                    build_in_place_tuple_image(&old_tuple_image, new_encoded, txn_id)?;
                stable_images.push(StableImage {
                    key,
                    old_tuple_image,
                    new_tuple_image,
                    page_id: rid.page_id,
                    slot_id: rid.slot_id,
                });
                new_rids.push(*rid);
            }
            None => {
                // Flush any accumulated stable images before the fallback path,
                // which will call record_delete + record_insert (WAL ordering).
                if !stable_images.is_empty() {
                    let batch_refs: Vec<StableUpdateBatchRef<'_>> = stable_images
                        .iter()
                        .map(|img| {
                            (
                                img.key.as_slice(),
                                img.old_tuple_image.as_slice(),
                                img.new_tuple_image.as_slice(),
                                img.page_id,
                                img.slot_id,
                            )
                        })
                        .collect();
                    let _ = txn.record_update_in_place_batch(conn_txn, table_def.id, &batch_refs);
                    stable_images.clear();
                }
                let new_rid = update_encoded_row_with_hint(
                    storage,
                    txn,
                    conn_txn,
                    table_def,
                    *rid,
                    new_encoded,
                    hint.as_deref_mut(),
                )?;
                new_rids.push(new_rid);
            }
        }
    }

    // Flush remaining stable images.
    if !stable_images.is_empty() {
        let batch_refs: Vec<StableUpdateBatchRef<'_>> = stable_images
            .iter()
            .map(|img| {
                (
                    img.key.as_slice(),
                    img.old_tuple_image.as_slice(),
                    img.new_tuple_image.as_slice(),
                    img.page_id,
                    img.slot_id,
                )
            })
            .collect();
        let _ = txn.record_update_in_place_batch(conn_txn, table_def.id, &batch_refs);
    }

    Ok(new_rids)
}

/// Applies strict-mode coercion to each value against its target column type.
pub(crate) fn coerce_values(
    values: Vec<Value>,
    columns: &[ColumnDef],
) -> Result<Vec<Value>, DbError> {
    values
        .into_iter()
        .zip(columns.iter())
        .map(|(v, col)| {
            let target = match col.col_type {
                ColumnType::Bool => DataType::Bool,
                ColumnType::Int => DataType::Int,
                ColumnType::BigInt => DataType::BigInt,
                ColumnType::Float => DataType::Real,
                ColumnType::Text => DataType::Text,
                ColumnType::Json => DataType::Json,
                ColumnType::Bytes => DataType::Bytes,
                ColumnType::Timestamp => DataType::Timestamp,
                ColumnType::Uuid => DataType::Uuid,
                ColumnType::Jsonb => DataType::Jsonb,
                ColumnType::Decimal => DataType::Decimal,
                ColumnType::Date => DataType::Date,
            };
            coerce(v, target, CoercionMode::Strict)
        })
        .collect()
}

/// Session-aware coercion for a single row.
///
/// When `ctx.strict_mode` is `true`, behaves identically to [`coerce_values`].
///
/// When `ctx.strict_mode` is `false`:
/// - Tries strict coercion first.
/// - If strict fails, tries permissive coercion.
/// - If permissive succeeds, emits warning 1265 and stores the permissive result.
/// - If permissive also fails, returns the permissive error (no warning emitted).
///
/// `row_num` is 1-based and statement-local (used in the warning message).
pub(crate) fn coerce_values_with_ctx(
    values: Vec<Value>,
    columns: &[ColumnDef],
    ctx: &mut SessionContext,
    row_num: usize,
) -> Result<Vec<Value>, DbError> {
    let mut out = Vec::with_capacity(values.len());
    for (v, col) in values.into_iter().zip(columns.iter()) {
        let target = match col.col_type {
            ColumnType::Bool => DataType::Bool,
            ColumnType::Int => DataType::Int,
            ColumnType::BigInt => DataType::BigInt,
            ColumnType::Float => DataType::Real,
            ColumnType::Text => DataType::Text,
            ColumnType::Json => DataType::Json,
            ColumnType::Bytes => DataType::Bytes,
            ColumnType::Timestamp => DataType::Timestamp,
            ColumnType::Uuid => DataType::Uuid,
            ColumnType::Jsonb => DataType::Jsonb,
            ColumnType::Decimal => DataType::Decimal,
            ColumnType::Date => DataType::Date,
        };

        if ctx.strict_mode {
            out.push(coerce(v, target, CoercionMode::Strict)?);
            continue;
        }

        // Strict first, permissive fallback.
        match coerce(v.clone(), target, CoercionMode::Strict) {
            Ok(strict_val) => {
                out.push(strict_val);
            }
            Err(_) => {
                let permissive_val = coerce(v, target, CoercionMode::Permissive)?;
                ctx.warn(
                    1265,
                    format!(
                        "Data truncated for column '{}' at row {}",
                        col.name, row_num
                    ),
                );
                out.push(permissive_val);
            }
        }
    }
    Ok(out)
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_catalog::schema::ColumnType;
    use axiomdb_storage::{MemoryStorage, Page, PageType};
    use axiomdb_wal::TxnManager;

    fn test_table_def(root_page_id: u64) -> TableDef {
        TableDef {
            id: 1,
            root_page_id,
            storage_layout: axiomdb_catalog::schema::TableStorageLayout::Heap,
            schema_name: "public".into(),
            table_name: "t".into(),
            schema_version: 1,
            immutable: false,
            persistence: axiomdb_catalog::schema::TablePersistence::Permanent,
        }
    }

    fn make_col(name: &str, col_type: ColumnType) -> ColumnDef {
        ColumnDef {
            table_id: 1,
            col_idx: 0,
            name: name.to_string(),
            col_type,
            nullable: true,
            auto_increment: false,
            type_len: 0,
            is_fixed_len: false,
            default_expr: None,
            on_update_expr: None,
            generated_expr: None,
            generated_stored: false,
        }
    }

    #[test]
    fn test_column_data_types_all_variants() {
        let cols = vec![
            make_col("a", ColumnType::Bool),
            make_col("b", ColumnType::Int),
            make_col("c", ColumnType::BigInt),
            make_col("d", ColumnType::Float),
            make_col("e", ColumnType::Text),
            make_col("f", ColumnType::Bytes),
            make_col("g", ColumnType::Timestamp),
            make_col("h", ColumnType::Uuid),
        ];
        let types = column_data_types(&cols);
        assert_eq!(
            types,
            vec![
                DataType::Bool,
                DataType::Int,
                DataType::BigInt,
                DataType::Real,
                DataType::Text,
                DataType::Bytes,
                DataType::Timestamp,
                DataType::Uuid,
            ]
        );
    }

    #[test]
    fn test_encode_rid() {
        let key = encode_rid(7, 3);
        // page_id=7 in little-endian 8 bytes, slot_id=3 in 2 bytes
        assert_eq!(&key[..8], &7u64.to_le_bytes());
        assert_eq!(&key[8..], &3u16.to_le_bytes());
    }

    #[test]
    fn test_encode_rid_zero() {
        let key = encode_rid(0, 0);
        assert_eq!(key, [0u8; 10]);
    }

    #[test]
    fn test_update_rows_preserve_rid_keeps_same_record_id_when_row_fits() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("table-test.wal");
        let storage = MemoryStorage::new();
        let root_page_id = storage.alloc_page(PageType::Data).unwrap();
        let root = Page::new(PageType::Data, root_page_id);
        storage.write_page(root_page_id, &root).unwrap();

        let table = test_table_def(root_page_id);
        let cols = vec![
            ColumnDef {
                table_id: 1,
                col_idx: 0,
                name: "id".into(),
                col_type: ColumnType::Int,
                nullable: false,
                auto_increment: false,
                type_len: 0,
                is_fixed_len: false,
                default_expr: None,
                on_update_expr: None,
                generated_expr: None,
                generated_stored: false,
            },
            ColumnDef {
                table_id: 1,
                col_idx: 1,
                name: "score".into(),
                col_type: ColumnType::Int,
                nullable: false,
                auto_increment: false,
                type_len: 0,
                is_fixed_len: false,
                default_expr: None,
                on_update_expr: None,
                generated_expr: None,
                generated_stored: false,
            },
        ];

        let txn = TxnManager::create(&wal).unwrap();
        let mut conn_txn = txn.begin().unwrap();
        let rid = TableEngine::insert_row(
            &storage,
            &txn,
            &mut conn_txn,
            &table,
            &cols,
            vec![Value::Int(1), Value::Int(10)],
        )
        .unwrap();

        let new_rids = TableEngine::update_rows_preserve_rid(
            &storage,
            &txn,
            &mut conn_txn,
            &table,
            &cols,
            vec![(rid, vec![Value::Int(1), Value::Int(11)])],
        )
        .unwrap();
        assert_eq!(new_rids, vec![rid], "same-slot rewrite must preserve RID");

        let row = TableEngine::read_row(&storage, &cols, rid)
            .unwrap()
            .unwrap();
        assert_eq!(row, vec![Value::Int(1), Value::Int(11)]);
    }

    #[test]
    fn test_scan_table_direct_masked_decode_can_skip_all_columns() {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("table-mask-test.wal");
        let storage = MemoryStorage::new();
        let root_page_id = storage.alloc_page(PageType::Data).unwrap();
        let root = Page::new(PageType::Data, root_page_id);
        storage.write_page(root_page_id, &root).unwrap();

        let table = test_table_def(root_page_id);
        let cols = vec![
            ColumnDef {
                table_id: 1,
                col_idx: 0,
                name: "id".into(),
                col_type: ColumnType::Int,
                nullable: false,
                auto_increment: false,
                type_len: 0,
                is_fixed_len: false,
                default_expr: None,
                on_update_expr: None,
                generated_expr: None,
                generated_stored: false,
            },
            ColumnDef {
                table_id: 1,
                col_idx: 1,
                name: "name".into(),
                col_type: ColumnType::Text,
                nullable: false,
                auto_increment: false,
                type_len: 0,
                is_fixed_len: false,
                default_expr: None,
                on_update_expr: None,
                generated_expr: None,
                generated_stored: false,
            },
        ];

        let txn = TxnManager::create(&wal).unwrap();
        let mut conn_txn = txn.begin().unwrap();
        TableEngine::insert_row(
            &storage,
            &txn,
            &mut conn_txn,
            &table,
            &cols,
            vec![Value::Int(7), Value::Text("alice".into())],
        )
        .unwrap();
        txn.commit(conn_txn).unwrap();

        let snap = txn.snapshot();
        let mask = [false, false];
        let rows =
            TableEngine::scan_table_direct(&storage, &table, &cols, snap, Some(&mask)).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, vec![Value::Null, Value::Null]);
    }
}
