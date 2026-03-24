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

use std::collections::HashMap;

use axiomdb_catalog::schema::{ColumnDef, ColumnType, TableDef};
use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot};
use axiomdb_storage::{HeapChain, StorageEngine, PAGE_SIZE};
use axiomdb_types::{
    codec::{decode_row, encode_row},
    coerce::{coerce, CoercionMode},
    DataType, Value,
};
use axiomdb_wal::TxnManager;

// ── TableEngine ───────────────────────────────────────────────────────────────

/// Stateless row storage interface for user tables.
///
/// Follows the same unit-struct pattern as [`HeapChain`]: all methods take
/// storage and transaction state as explicit parameters.
pub struct TableEngine;

impl TableEngine {
    /// Returns all MVCC-visible rows in the table, decoded as `Vec<Value>`.
    ///
    /// Rows are returned in heap chain order (root page first, slot order within
    /// each page). Dead slots and rows not visible to `snap` are excluded.
    ///
    /// An empty table returns `Ok(vec![])` — not an error.
    ///
    /// `columns` must be sorted ascending by `col_idx` (catalog declaration order).
    ///
    /// # Errors
    /// - [`DbError::ParseError`] — a stored row is structurally invalid (corruption).
    /// - I/O errors from storage reads.
    pub fn scan_table(
        storage: &dyn StorageEngine,
        table_def: &TableDef,
        columns: &[ColumnDef],
        snap: TransactionSnapshot,
    ) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
        let col_types = column_data_types(columns);
        let raw_rows = HeapChain::scan_visible(storage, table_def.data_root_page_id, snap)?;

        let mut result = Vec::with_capacity(raw_rows.len());
        for (page_id, slot_id, bytes) in raw_rows {
            let values = decode_row(&bytes, &col_types)?;
            result.push((RecordId { page_id, slot_id }, values));
        }
        Ok(result)
    }

    /// Reads a single row by `RecordId` and decodes it into `Vec<Value>`.
    ///
    /// Returns `None` if the slot has been deleted (tombstone).
    ///
    /// # Errors
    /// - [`DbError::ParseError`] — the row bytes are structurally invalid.
    /// - I/O errors from storage reads.
    pub fn read_row(
        storage: &dyn StorageEngine,
        columns: &[ColumnDef],
        rid: RecordId,
    ) -> Result<Option<Vec<Value>>, DbError> {
        match HeapChain::read_row(storage, rid.page_id, rid.slot_id)? {
            None => Ok(None),
            Some(bytes) => {
                let col_types = column_data_types(columns);
                let values = decode_row(&bytes, &col_types)?;
                Ok(Some(values))
            }
        }
    }

    /// Encodes and inserts a row into the table heap, WAL-logging the insert.
    ///
    /// Applies implicit coercion (strict mode) from each value to the declared
    /// column type before encoding. For example, `Text("42")` into an `INT`
    /// column becomes `Int(42)`.
    ///
    /// Must be called inside an active transaction (`txn.begin()` already called).
    ///
    /// # Errors
    /// - [`DbError::TypeMismatch`] — `values.len() != columns.len()`.
    /// - [`DbError::InvalidCoercion`] — a value cannot be coerced to the column type.
    /// - [`DbError::NoActiveTransaction`] — no transaction is active.
    /// - I/O errors from storage or WAL writes.
    pub fn insert_row(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        values: Vec<Value>,
    ) -> Result<RecordId, DbError> {
        if values.len() != columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", columns.len()),
                got: format!("{} values", values.len()),
            });
        }

        let col_types = column_data_types(columns);
        let coerced = coerce_values(values, columns)?;
        let encoded = encode_row(&coerced, &col_types)?;

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let (page_id, slot_id) =
            HeapChain::insert(storage, table_def.data_root_page_id, &encoded, txn_id)?;

        let key = encode_rid(page_id, slot_id);
        txn.record_insert(table_def.id, &key, &encoded, page_id, slot_id)?;

        Ok(RecordId { page_id, slot_id })
    }

    /// Encodes and inserts **multiple rows** into the table heap in one pass,
    /// WAL-logging each insert.
    ///
    /// This is the batch counterpart of [`insert_row`]. It calls
    /// [`HeapChain::insert_batch`] which loads each heap page exactly once
    /// regardless of how many rows are written to it — reducing per-row
    /// `read_page` + `write_page` calls from O(N) to O(pages).
    ///
    /// ## Encoding phase (fail-fast)
    ///
    /// All rows are coerced and encoded before any heap or WAL write. If any
    /// row fails type coercion, the function returns an error and the heap is
    /// untouched.
    ///
    /// ## WAL ordering
    ///
    /// `HeapChain::insert_batch()` writes pages before returning the
    /// `(page_id, slot_id)` pairs. `record_insert()` is then called for each
    /// row. Both heap and WAL writes are in the BufWriter / mmap (not yet
    /// durable). Durability comes from `TxnManager::commit()`.
    ///
    /// Must be called inside an active transaction.
    ///
    /// # Errors
    /// - [`DbError::TypeMismatch`] — any row has wrong column count.
    /// - [`DbError::InvalidCoercion`] — any value cannot be coerced.
    /// - [`DbError::NoActiveTransaction`] — no transaction is active.
    /// - I/O errors from storage or WAL writes.
    pub fn insert_rows_batch(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        batch: &[Vec<Value>],
    ) -> Result<Vec<RecordId>, DbError> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }

        let col_types = column_data_types(columns);

        // ── Encode all rows first (fail-fast, no heap writes yet) ─────────────
        let encoded_rows: Vec<Vec<u8>> = batch
            .iter()
            .map(|values| {
                let values = values.clone();
                if values.len() != columns.len() {
                    return Err(DbError::TypeMismatch {
                        expected: format!("{} columns", columns.len()),
                        got: format!("{} values", values.len()),
                    });
                }
                let coerced = coerce_values(values, columns)?;
                encode_row(&coerced, &col_types)
            })
            .collect::<Result<_, _>>()?;

        // ── Insert all rows into the heap in one batch pass ───────────────────
        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let phys_locs =
            HeapChain::insert_batch(storage, table_def.data_root_page_id, &encoded_rows, txn_id)?;

        // ── WAL: one PageWrite entry per affected page (Phase 3.18) ─────────
        // Instead of N Insert entries (one per row), emit one PageWrite per
        // unique page touched by the batch. Each entry carries the full page
        // post-image plus the list of slot_ids inserted by this transaction,
        // so crash recovery can undo uncommitted writes at slot granularity.
        //
        // For a 10K-row insert spanning ~625 pages (at 16-row capacity each):
        //   Before: 10K serialize_into() calls + 10K CRC32c computations
        //   After:  ~625 serialize_into() calls + ~625 CRC32c computations
        //
        // The page reads below (read_page per unique page) are mmap cache hits
        // because HeapChain::insert_batch() just wrote those same pages.

        // Group slot_ids by page_id.
        let mut page_slot_map: HashMap<u64, Vec<u16>> = HashMap::new();
        for &(page_id, slot_id) in &phys_locs {
            page_slot_map.entry(page_id).or_default().push(slot_id);
        }

        // Sort by page_id for deterministic WAL ordering.
        let mut sorted_pages: Vec<(u64, Vec<u16>)> = page_slot_map.into_iter().collect();
        sorted_pages.sort_unstable_by_key(|(page_id, _)| *page_id);

        // Read final page bytes for each affected page (mmap cache hit — just written).
        let mut page_write_args: Vec<(u64, [u8; PAGE_SIZE], Vec<u16>)> =
            Vec::with_capacity(sorted_pages.len());
        for (page_id, slot_ids) in sorted_pages {
            let page_bytes = *storage.read_page(page_id)?.as_bytes();
            page_write_args.push((page_id, page_bytes, slot_ids));
        }

        // Emit one PageWrite WAL entry per affected page.
        let pw_refs: Vec<(u64, &[u8; PAGE_SIZE], &[u16])> = page_write_args
            .iter()
            .map(|(pid, bytes, slots)| (*pid, bytes, slots.as_slice()))
            .collect();
        txn.record_page_writes(table_def.id, &pw_refs)?;

        let result = phys_locs
            .iter()
            .map(|(page_id, slot_id)| RecordId {
                page_id: *page_id,
                slot_id: *slot_id,
            })
            .collect();

        Ok(result)
    }

    /// Stamps an MVCC deletion on the row at `record_id`, WAL-logging the delete.
    ///
    /// The old row bytes are read before deletion to include as `old_value` in
    /// the WAL entry for crash recovery.
    ///
    /// Must be called inside an active transaction.
    ///
    /// # Errors
    /// - [`DbError::AlreadyDeleted`] — the slot is already dead.
    /// - [`DbError::InvalidSlot`] — `record_id` points to a non-existent slot.
    /// - [`DbError::NoActiveTransaction`] — no transaction is active.
    /// - I/O errors from storage or WAL writes.
    pub fn delete_row(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        record_id: RecordId,
    ) -> Result<(), DbError> {
        // Read old bytes BEFORE deletion — read_tuple returns None on dead slots.
        let old_bytes = HeapChain::read_row(storage, record_id.page_id, record_id.slot_id)?.ok_or(
            DbError::AlreadyDeleted {
                page_id: record_id.page_id,
                slot_id: record_id.slot_id,
            },
        )?;

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        HeapChain::delete(storage, record_id.page_id, record_id.slot_id, txn_id)?;

        let key = encode_rid(record_id.page_id, record_id.slot_id);
        txn.record_delete(
            table_def.id,
            &key,
            &old_bytes,
            record_id.page_id,
            record_id.slot_id,
        )?;

        Ok(())
    }

    /// Deletes multiple rows in a single pass over the heap.
    ///
    /// Each heap page is read and written **exactly once** regardless of how
    /// many rows are deleted from it — compared to N × `delete_row()` calls
    /// which do 3 page operations per row (read + read + write).
    ///
    /// WAL entries are emitted after the page writes, preserving the invariant
    /// that `write_page()` always precedes `record_delete()`.
    ///
    /// Returns the number of rows deleted.
    pub fn delete_rows_batch(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        rids: &[RecordId],
    ) -> Result<u64, DbError> {
        if rids.is_empty() {
            return Ok(0);
        }

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let raw_rids: Vec<(u64, u16)> = rids.iter().map(|r| (r.page_id, r.slot_id)).collect();

        // Batch-delete on the heap: each page read+written once.
        let deleted = HeapChain::delete_batch(storage, &raw_rids, txn_id)?;

        // WAL entries: one per row, after all page writes (ordering invariant).
        for (page_id, slot_id, old_bytes) in &deleted {
            let key = encode_rid(*page_id, *slot_id);
            txn.record_delete(table_def.id, &key, old_bytes, *page_id, *slot_id)?;
        }

        Ok(deleted.len() as u64)
    }

    /// Replaces the row at `record_id` with `new_values`, WAL-logging both the
    /// delete and the insert.
    ///
    /// Implemented as `delete_row` + `insert_row` to avoid the same-page
    /// assumption of `TxnManager::record_update`. The returned `RecordId` is
    /// the physical location of the new row, which may differ from `record_id`
    /// if the old page was full and the chain grew.
    ///
    /// Must be called inside an active transaction.
    ///
    /// # Errors
    /// - [`DbError::TypeMismatch`] — `new_values.len() != columns.len()`.
    /// - [`DbError::InvalidCoercion`] — a new value cannot be coerced to the column type.
    /// - [`DbError::AlreadyDeleted`] — the old row slot is already dead.
    /// - [`DbError::NoActiveTransaction`] — no transaction is active.
    /// - I/O errors from storage or WAL writes.
    pub fn update_row(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        record_id: RecordId,
        new_values: Vec<Value>,
    ) -> Result<RecordId, DbError> {
        if new_values.len() != columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", columns.len()),
                got: format!("{} values", new_values.len()),
            });
        }

        // Read old bytes BEFORE any mutation (slot becomes dead after delete).
        let old_bytes = HeapChain::read_row(storage, record_id.page_id, record_id.slot_id)?.ok_or(
            DbError::AlreadyDeleted {
                page_id: record_id.page_id,
                slot_id: record_id.slot_id,
            },
        )?;

        let col_types = column_data_types(columns);
        let coerced = coerce_values(new_values, columns)?;
        let new_encoded = encode_row(&coerced, &col_types)?;

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;

        // Step 1: stamp deletion on the old row, WAL-log the delete.
        HeapChain::delete(storage, record_id.page_id, record_id.slot_id, txn_id)?;
        let old_key = encode_rid(record_id.page_id, record_id.slot_id);
        txn.record_delete(
            table_def.id,
            &old_key,
            &old_bytes,
            record_id.page_id,
            record_id.slot_id,
        )?;

        // Step 2: insert the new row into the chain, WAL-log the insert.
        let (new_page_id, new_slot_id) =
            HeapChain::insert(storage, table_def.data_root_page_id, &new_encoded, txn_id)?;
        let new_key = encode_rid(new_page_id, new_slot_id);
        txn.record_insert(
            table_def.id,
            &new_key,
            &new_encoded,
            new_page_id,
            new_slot_id,
        )?;

        Ok(RecordId {
            page_id: new_page_id,
            slot_id: new_slot_id,
        })
    }
}

// ── Private helpers ───────────────────────────────────────────────────────────

/// Extracts `DataType` from each `ColumnDef` in declaration order.
///
/// `ColumnType` (compact catalog representation) maps to `DataType`
/// (full in-memory type used by the row codec and expression evaluator).
fn column_data_types(columns: &[ColumnDef]) -> Vec<DataType> {
    columns
        .iter()
        .map(|c| match c.col_type {
            ColumnType::Bool => DataType::Bool,
            ColumnType::Int => DataType::Int,
            ColumnType::BigInt => DataType::BigInt,
            ColumnType::Float => DataType::Real,
            ColumnType::Text => DataType::Text,
            ColumnType::Bytes => DataType::Bytes,
            ColumnType::Timestamp => DataType::Timestamp,
            ColumnType::Uuid => DataType::Uuid,
        })
        .collect()
}

/// Encodes a `RecordId` as a 10-byte WAL key: `[page_id:8 LE][slot_id:2 LE]`.
fn encode_rid(page_id: u64, slot_id: u16) -> [u8; 10] {
    let mut buf = [0u8; 10];
    buf[..8].copy_from_slice(&page_id.to_le_bytes());
    buf[8..].copy_from_slice(&slot_id.to_le_bytes());
    buf
}

/// Applies strict-mode coercion to each value against its target column type.
fn coerce_values(values: Vec<Value>, columns: &[ColumnDef]) -> Result<Vec<Value>, DbError> {
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
                ColumnType::Bytes => DataType::Bytes,
                ColumnType::Timestamp => DataType::Timestamp,
                ColumnType::Uuid => DataType::Uuid,
            };
            coerce(v, target, CoercionMode::Strict)
        })
        .collect()
}

// ── Unit tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_catalog::schema::ColumnType;

    fn make_col(name: &str, col_type: ColumnType) -> ColumnDef {
        ColumnDef {
            table_id: 1,
            col_idx: 0,
            name: name.to_string(),
            col_type,
            nullable: true,
            auto_increment: false,
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
}
