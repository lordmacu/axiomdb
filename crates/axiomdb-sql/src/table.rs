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

use axiomdb_catalog::schema::{ColumnDef, ColumnType, TableDef};
use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot};
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

use crate::session::SessionContext;

type StableUpdateBatchRef<'a> = (&'a [u8], &'a [u8], &'a [u8], u64, u16);

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
    /// Scans all visible rows in the table and decodes them.
    ///
    /// # Errors
    /// - [`DbError::ParseError`] — a stored row is structurally invalid (corruption).
    /// - I/O errors from storage reads.
    ///
    /// `column_mask` controls which columns are decoded:
    /// - `None` — decode all columns (default, same as before).
    /// - `Some(mask)` — decode only columns where `mask[i]` is `true`; skipped
    ///   columns have `Value::Null` in the output. This eliminates allocation and
    ///   parsing cost for columns not referenced by the query (lazy column decode).
    ///
    /// When `mask` is all-`true`, [`decode_row`] is used directly so there is no
    /// overhead compared to passing `None`.
    pub fn scan_table(
        storage: &mut dyn StorageEngine,
        table_def: &TableDef,
        columns: &[ColumnDef],
        snap: TransactionSnapshot,
        column_mask: Option<&[bool]>,
    ) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
        Self::scan_table_direct(storage, table_def, columns, snap, column_mask)
    }

    /// Like [`scan_table`] but inlines the heap traversal and decodes rows
    /// directly from page bytes — eliminating the intermediate `Vec<u8>`
    /// allocation (`.to_vec()`) per row that `HeapChain::scan_visible` produces.
    ///
    /// On a 50K-row table this saves ~50 000 heap allocations, reducing
    /// allocation pressure from the per-row copy. Page prefetching is
    /// included: the next heap chain page is hinted before decoding the
    /// current page's rows, overlapping I/O with decode on cold caches.
    ///
    /// Falls back to [`scan_table`] when `column_mask` is `Some` (masked
    /// decode needs a separate code path that isn't worth duplicating here).
    pub fn scan_table_direct(
        storage: &mut dyn StorageEngine,
        table_def: &TableDef,
        columns: &[ColumnDef],
        snap: TransactionSnapshot,
        column_mask: Option<&[bool]>,
    ) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
        let col_types = column_data_types(columns);
        let masked_decode = column_mask.filter(|mask| !mask.iter().all(|&b| b));
        let mut result = Vec::new();
        let mut current = table_def.data_root_page_id;

        while current != 0 {
            let raw = *storage.read_page(current)?.as_bytes();
            let page = Page::from_bytes(raw)?;
            let next = heap_chain::chain_next_page(&page);

            // Prefetch next page while processing current page's rows.
            if next != 0 {
                storage.prefetch_hint(next, 1);
            }

            let num = num_slots(&page);
            for slot_id in 0..num {
                let entry = read_slot(&page, slot_id);
                if entry.is_dead() {
                    continue;
                }
                let off = entry.offset as usize;
                let len = entry.length as usize;
                let bytes = &page.as_bytes()[off..off + len];
                let header: &RowHeader = bytemuck::from_bytes(&bytes[..size_of::<RowHeader>()]);
                if !header.is_visible(&snap) {
                    continue;
                }
                // Decode directly from page bytes — no .to_vec().
                let row_data = &bytes[size_of::<RowHeader>()..];
                let values = if let Some(mask) = masked_decode {
                    decode_row_masked(row_data, &col_types, mask)?
                } else {
                    decode_row(row_data, &col_types)?
                };
                result.push((
                    RecordId {
                        page_id: current,
                        slot_id,
                    },
                    values,
                ));
            }

            current = next;
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

    /// Reads multiple rows by `RecordId` in a single pass over the heap,
    /// grouping reads by page for I/O locality.
    ///
    /// Returns a vector parallel to `rids`:
    /// - `Some(values)` if the slot is alive
    /// - `None` if the slot is dead
    ///
    /// For N rows across P pages this is O(P) page reads instead of O(N).
    pub fn read_rows_batch(
        storage: &dyn StorageEngine,
        columns: &[ColumnDef],
        rids: &[RecordId],
    ) -> Result<Vec<Option<Vec<Value>>>, DbError> {
        if rids.is_empty() {
            return Ok(Vec::new());
        }
        let raw_rids: Vec<(u64, u16)> = rids.iter().map(|r| (r.page_id, r.slot_id)).collect();
        let raw_results = HeapChain::read_rows_batch(storage, &raw_rids)?;
        let col_types = column_data_types(columns);
        raw_results
            .into_iter()
            .map(|raw| match raw {
                None => Ok(None),
                Some(bytes) => Ok(Some(decode_row(&bytes, &col_types)?)),
            })
            .collect()
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

    /// Inserts one row using an optional heap-tail hint for O(1) tail lookup.
    ///
    /// If `hint` is `Some(...)`, the tail page is resolved via
    /// [`HeapChain::insert_with_hint`] instead of walking from the root.
    /// The hint is updated in place after the insert so the caller can pass the
    /// same reference to subsequent calls and accumulate tail state.
    ///
    /// Use this in hot loops (ctx per-row insert paths) to avoid O(N²) behavior.
    pub fn insert_row_with_hint(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        values: Vec<Value>,
        hint: Option<&mut HeapAppendHint>,
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
        let (page_id, slot_id) = HeapChain::insert_with_hint(
            storage,
            table_def.data_root_page_id,
            &encoded,
            txn_id,
            hint,
        )?;
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

        // ── WAL: one compact PageWrite entry per affected page ───────────────
        // Group slot_ids by page_id. Each PageWrite entry carries only the
        // slot_ids (not the 16 KB page image), reducing WAL from ~820 KB to
        // ~20 KB per 10 K-row batch — a 40× reduction.
        // Crash recovery only needs slot_ids to mark inserted slots dead on undo.
        let mut page_slot_map: std::collections::HashMap<u64, Vec<u16>> =
            std::collections::HashMap::new();
        for &(page_id, slot_id) in &phys_locs {
            page_slot_map.entry(page_id).or_default().push(slot_id);
        }

        // Sort by page_id for deterministic WAL ordering.
        let mut sorted_pages: Vec<(u64, Vec<u16>)> = page_slot_map.into_iter().collect();
        sorted_pages.sort_unstable_by_key(|(page_id, _)| *page_id);

        // Emit one PageWrite WAL entry per affected page.
        let pw_refs: Vec<(u64, &[u16])> = sorted_pages
            .iter()
            .map(|(pid, slots)| (*pid, slots.as_slice()))
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
        let deleted =
            HeapChain::delete_batch(storage, table_def.data_root_page_id, &raw_rids, txn_id)?;

        // WAL entries: one per row, after all page writes (ordering invariant).
        for (page_id, slot_id, old_bytes) in &deleted {
            let key = encode_rid(*page_id, *slot_id);
            txn.record_delete(table_def.id, &key, old_bytes, *page_id, *slot_id)?;
        }

        Ok(deleted.len() as u64)
    }

    /// Updates multiple rows in two batch passes: delete all old slots, then
    /// insert all new rows.
    ///
    /// Inspired by OceanBase's dual-row buffer (`ObDASUpdIterator`) and
    /// MariaDB's `ha_bulk_update_row()`: accumulate all (old, new) pairs first,
    /// then flush as a single delete_batch + insert_batch operation.
    ///
    /// ## Performance
    ///
    /// Per-row `update_row()` does ~3 page ops per row (read + read+write for
    /// delete + read+write for insert). This function does O(P) ops for P pages:
    /// - `delete_rows_batch`: 1 read + 1 write per page holding old rows
    /// - `insert_rows_batch`: 1 read + 1 write per page receiving new rows
    ///
    /// For 5,000 rows across 50 pages: ~200 page ops vs ~15,000.
    ///
    /// ## WAL ordering
    ///
    /// All deletes (heap write + WAL) happen before all inserts, ensuring that
    /// crash recovery can undo the update by undoing inserts (killing new slots)
    /// then undoing deletes (resurrecting old slots) in reverse WAL order.
    ///
    /// Must be called inside an active transaction.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] — no transaction is active.
    /// - [`DbError::TypeMismatch`] — any new row has wrong column count.
    /// - I/O errors from storage or WAL writes.
    pub fn update_rows_batch(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        updates: Vec<(RecordId, Vec<Value>)>,
    ) -> Result<u64, DbError> {
        if updates.is_empty() {
            return Ok(0);
        }

        let (rids, new_values): (Vec<RecordId>, Vec<Vec<Value>>) = updates.into_iter().unzip();

        // Phase 1: batch-delete all old rows (O(P) page I/O for P pages).
        // Reads each page once, marks all targeted slots dead, writes once.
        Self::delete_rows_batch(storage, txn, table_def, &rids)?;

        // Phase 2: batch-insert all new rows (O(P') page I/O for P' pages).
        // Encodes all rows first (fail-fast), then appends in one heap pass.
        Self::insert_rows_batch(storage, txn, table_def, columns, &new_values)?;

        Ok(rids.len() as u64)
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

        let col_types = column_data_types(columns);
        let coerced = coerce_values(new_values, columns)?;
        let new_encoded = encode_row(&coerced, &col_types)?;
        update_encoded_row_with_hint(storage, txn, table_def, record_id, &new_encoded, None)
    }

    /// Updates one row using a heap-tail hint for the insert half.
    ///
    /// The delete half is unchanged; the insert half calls
    /// [`HeapChain::insert_with_hint`] to avoid re-walking the chain from root
    /// on each iteration of a bulk UPDATE loop.
    pub fn update_row_with_hint(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        record_id: RecordId,
        new_values: Vec<Value>,
        hint: Option<&mut HeapAppendHint>,
    ) -> Result<RecordId, DbError> {
        if new_values.len() != columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", columns.len()),
                got: format!("{} values", new_values.len()),
            });
        }
        let col_types = column_data_types(columns);
        let coerced = coerce_values(new_values, columns)?;
        let new_encoded = encode_row(&coerced, &col_types)?;
        update_encoded_row_with_hint(storage, txn, table_def, record_id, &new_encoded, hint)
    }

    // ── ctx-aware write variants (session strict_mode + warning emission) ─────

    /// Session-aware insert: applies strict or permissive coercion depending on
    /// `ctx.strict_mode`, emitting warning 1265 on permissive fallback.
    ///
    /// `row_num` is 1-based and statement-local — used in the warning message so
    /// multi-row `INSERT VALUES` callers can pass the loop counter.
    pub fn insert_row_with_ctx(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        values: Vec<Value>,
        row_num: usize,
    ) -> Result<RecordId, DbError> {
        if values.len() != columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", columns.len()),
                got: format!("{} values", values.len()),
            });
        }
        let col_types = column_data_types(columns);
        let coerced = coerce_values_with_ctx(values, columns, ctx, row_num)?;
        let encoded = encode_row(&coerced, &col_types)?;
        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;

        // Phase 5.18: pull heap-tail hint from the session cache, use it for O(1)
        // tail lookup, and write the updated hint back after the insert.
        let mut hint_opt = ctx.get_heap_tail_hint(table_def.id, table_def.data_root_page_id);
        let (page_id, slot_id) = HeapChain::insert_with_hint(
            storage,
            table_def.data_root_page_id,
            &encoded,
            txn_id,
            hint_opt.as_mut(),
        )?;
        if let Some(h) = hint_opt {
            ctx.set_heap_tail_hint(table_def.id, h.root_page_id, h.tail_page_id);
        } else {
            // No existing hint — seed one for the next call.
            ctx.set_heap_tail_hint(table_def.id, table_def.data_root_page_id, page_id);
        }

        let key = encode_rid(page_id, slot_id);
        txn.record_insert(table_def.id, &key, &encoded, page_id, slot_id)?;
        Ok(RecordId { page_id, slot_id })
    }

    /// Session-aware batch insert: applies strict or permissive coercion per row,
    /// emitting warning 1265 (with 1-based row numbers) on permissive fallback.
    pub fn insert_rows_batch_with_ctx(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        batch: &[Vec<Value>],
    ) -> Result<Vec<RecordId>, DbError> {
        if batch.is_empty() {
            return Ok(Vec::new());
        }
        let col_types = column_data_types(columns);

        let encoded_rows: Vec<Vec<u8>> = batch
            .iter()
            .enumerate()
            .map(|(i, values)| {
                let values = values.clone();
                if values.len() != columns.len() {
                    return Err(DbError::TypeMismatch {
                        expected: format!("{} columns", columns.len()),
                        got: format!("{} values", values.len()),
                    });
                }
                let coerced = coerce_values_with_ctx(values, columns, ctx, i + 1)?;
                encode_row(&coerced, &col_types)
            })
            .collect::<Result<_, _>>()?;

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let phys_locs =
            HeapChain::insert_batch(storage, table_def.data_root_page_id, &encoded_rows, txn_id)?;

        let mut page_slot_map: std::collections::HashMap<u64, Vec<u16>> =
            std::collections::HashMap::new();
        for &(page_id, slot_id) in &phys_locs {
            page_slot_map.entry(page_id).or_default().push(slot_id);
        }
        let mut sorted_pages: Vec<(u64, Vec<u16>)> = page_slot_map.into_iter().collect();
        sorted_pages.sort_unstable_by_key(|(page_id, _)| *page_id);
        let pw_refs: Vec<(u64, &[u16])> = sorted_pages
            .iter()
            .map(|(pid, slots)| (*pid, slots.as_slice()))
            .collect();
        txn.record_page_writes(table_def.id, &pw_refs)?;

        Ok(phys_locs
            .iter()
            .map(|(page_id, slot_id)| RecordId {
                page_id: *page_id,
                slot_id: *slot_id,
            })
            .collect())
    }

    /// Session-aware single-row update: applies strict or permissive coercion,
    /// emitting warning 1265 on permissive fallback.
    pub fn update_row_with_ctx(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        record_id: RecordId,
        new_values: Vec<Value>,
    ) -> Result<RecordId, DbError> {
        if new_values.len() != columns.len() {
            return Err(DbError::TypeMismatch {
                expected: format!("{} columns", columns.len()),
                got: format!("{} values", new_values.len()),
            });
        }
        let col_types = column_data_types(columns);
        let coerced = coerce_values_with_ctx(new_values, columns, ctx, 1)?;
        let new_encoded = encode_row(&coerced, &col_types)?;
        // Phase 5.18: use session heap-tail hint for the insert half of UPDATE.
        let mut hint_opt = ctx.get_heap_tail_hint(table_def.id, table_def.data_root_page_id);
        let new_rid = update_encoded_row_with_hint(
            storage,
            txn,
            table_def,
            record_id,
            &new_encoded,
            hint_opt.as_mut(),
        )?;
        if let Some(h) = hint_opt {
            ctx.set_heap_tail_hint(table_def.id, h.root_page_id, h.tail_page_id);
        } else {
            ctx.set_heap_tail_hint(table_def.id, table_def.data_root_page_id, new_rid.page_id);
        }
        Ok(new_rid)
    }

    /// Session-aware batch update: applies strict or permissive coercion per row
    /// (1-based row numbers for warning messages).
    pub fn update_rows_batch_with_ctx(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        updates: Vec<(RecordId, Vec<Value>)>,
    ) -> Result<u64, DbError> {
        if updates.is_empty() {
            return Ok(0);
        }
        let (rids, new_values_vec): (Vec<RecordId>, Vec<Vec<Value>>) = updates.into_iter().unzip();

        // Delete all old rows first.
        Self::delete_rows_batch(storage, txn, table_def, &rids)?;

        // Encode all new rows with ctx-aware coercion, then batch-insert.
        let col_types = column_data_types(columns);
        let encoded_rows: Vec<Vec<u8>> = new_values_vec
            .into_iter()
            .enumerate()
            .map(|(i, values)| {
                if values.len() != columns.len() {
                    return Err(DbError::TypeMismatch {
                        expected: format!("{} columns", columns.len()),
                        got: format!("{} values", values.len()),
                    });
                }
                let coerced = coerce_values_with_ctx(values, columns, ctx, i + 1)?;
                encode_row(&coerced, &col_types)
            })
            .collect::<Result<_, _>>()?;

        let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
        let phys_locs =
            HeapChain::insert_batch(storage, table_def.data_root_page_id, &encoded_rows, txn_id)?;

        let mut page_slot_map: std::collections::HashMap<u64, Vec<u16>> =
            std::collections::HashMap::new();
        for &(page_id, slot_id) in &phys_locs {
            page_slot_map.entry(page_id).or_default().push(slot_id);
        }
        let mut sorted_pages: Vec<(u64, Vec<u16>)> = page_slot_map.into_iter().collect();
        sorted_pages.sort_unstable_by_key(|(page_id, _)| *page_id);
        let pw_refs: Vec<(u64, &[u16])> = sorted_pages
            .iter()
            .map(|(pid, slots)| (*pid, slots.as_slice()))
            .collect();
        txn.record_page_writes(table_def.id, &pw_refs)?;

        Ok(rids.len() as u64)
    }

    /// Updates rows while attempting to preserve each row's `RecordId`.
    ///
    /// For every row, AxiomDB first tries a same-slot rewrite in the heap. If
    /// the new encoded row fits in the existing slot capacity, the row keeps the
    /// same `(page_id, slot_id)` and the WAL records an `UpdateInPlace`. If the
    /// row does not fit, this falls back to the existing delete+insert path and
    /// returns a new `RecordId`.
    ///
    /// The returned vector is parallel to `updates`.
    pub fn update_rows_preserve_rid(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        updates: Vec<(RecordId, Vec<Value>)>,
    ) -> Result<Vec<RecordId>, DbError> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        let col_types = column_data_types(columns);
        let prepared: Vec<(RecordId, Vec<u8>)> = updates
            .into_iter()
            .map(|(rid, values)| {
                if values.len() != columns.len() {
                    return Err(DbError::TypeMismatch {
                        expected: format!("{} columns", columns.len()),
                        got: format!("{} values", values.len()),
                    });
                }
                let coerced = coerce_values(values, columns)?;
                let encoded = encode_row(&coerced, &col_types)?;
                Ok((rid, encoded))
            })
            .collect::<Result<_, _>>()?;

        apply_prepared_updates_preserve_rid(storage, txn, table_def, prepared, None)
    }

    /// Session-aware stable-RID batch update.
    ///
    /// Uses the same preserve-RID fast path as [`update_rows_preserve_rid`], but
    /// applies strict/permissive coercion with warning emission through `ctx`.
    pub fn update_rows_preserve_rid_with_ctx(
        storage: &mut dyn StorageEngine,
        txn: &mut TxnManager,
        table_def: &TableDef,
        columns: &[ColumnDef],
        ctx: &mut SessionContext,
        updates: Vec<(RecordId, Vec<Value>)>,
    ) -> Result<Vec<RecordId>, DbError> {
        if updates.is_empty() {
            return Ok(Vec::new());
        }
        let col_types = column_data_types(columns);
        let prepared: Vec<(RecordId, Vec<u8>)> = updates
            .into_iter()
            .enumerate()
            .map(|(i, (rid, values))| {
                if values.len() != columns.len() {
                    return Err(DbError::TypeMismatch {
                        expected: format!("{} columns", columns.len()),
                        got: format!("{} values", values.len()),
                    });
                }
                let coerced = coerce_values_with_ctx(values, columns, ctx, i + 1)?;
                let encoded = encode_row(&coerced, &col_types)?;
                Ok((rid, encoded))
            })
            .collect::<Result<_, _>>()?;

        let mut hint_opt = ctx.get_heap_tail_hint(table_def.id, table_def.data_root_page_id);
        let original_rids: Vec<RecordId> = prepared.iter().map(|(rid, _)| *rid).collect();
        let new_rids = apply_prepared_updates_preserve_rid(
            storage,
            txn,
            table_def,
            prepared,
            hint_opt.as_mut(),
        )?;
        if let Some(h) = hint_opt {
            ctx.set_heap_tail_hint(table_def.id, h.root_page_id, h.tail_page_id);
        } else if let Some(last_fallback) = original_rids
            .iter()
            .zip(new_rids.iter())
            .rev()
            .find_map(|(old_rid, new_rid)| (old_rid != new_rid).then_some(*new_rid))
        {
            ctx.set_heap_tail_hint(
                table_def.id,
                table_def.data_root_page_id,
                last_fallback.page_id,
            );
        }
        Ok(new_rids)
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

/// Decodes a raw row byte slice into a `Vec<Value>` using column definitions.
///
/// Public helper for modules (e.g., `fk_enforcement`) that read rows from the
/// heap and need to decode them without going through `scan_table`.
pub fn decode_row_from_bytes(bytes: &[u8], columns: &[ColumnDef]) -> Result<Vec<Value>, DbError> {
    let col_types = column_data_types(columns);
    decode_row(bytes, &col_types)
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
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
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
        table_def.id,
        &old_key,
        &old_bytes,
        record_id.page_id,
        record_id.slot_id,
    )?;

    let (new_page_id, new_slot_id) = match hint {
        Some(h) => HeapChain::insert_with_hint(
            storage,
            table_def.data_root_page_id,
            new_encoded,
            txn_id,
            Some(h),
        )?,
        None => HeapChain::insert(storage, table_def.data_root_page_id, new_encoded, txn_id)?,
    };
    let new_key = encode_rid(new_page_id, new_slot_id);
    txn.record_insert(
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
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    table_def: &TableDef,
    prepared: Vec<(RecordId, Vec<u8>)>,
    mut hint: Option<&mut HeapAppendHint>,
) -> Result<Vec<RecordId>, DbError> {
    if prepared.is_empty() {
        return Ok(Vec::new());
    }

    let txn_id = txn.active_txn_id().ok_or(DbError::NoActiveTransaction)?;
    let rewrite_results = HeapChain::rewrite_batch_same_slot(
        storage,
        table_def.data_root_page_id,
        &prepared,
        txn_id,
    )?;

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

    for ((rid, new_encoded), rewrite_result) in prepared.iter().zip(rewrite_results.into_iter()) {
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
                    txn.record_update_in_place_batch(table_def.id, &batch_refs)?;
                    stable_images.clear();
                }
                let new_rid = update_encoded_row_with_hint(
                    storage,
                    txn,
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
        txn.record_update_in_place_batch(table_def.id, &batch_refs)?;
    }

    Ok(new_rids)
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
fn coerce_values_with_ctx(
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
            ColumnType::Bytes => DataType::Bytes,
            ColumnType::Timestamp => DataType::Timestamp,
            ColumnType::Uuid => DataType::Uuid,
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
            data_root_page_id: root_page_id,
            schema_name: "public".into(),
            table_name: "t".into(),
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
        let mut storage = MemoryStorage::new();
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
            },
            ColumnDef {
                table_id: 1,
                col_idx: 1,
                name: "score".into(),
                col_type: ColumnType::Int,
                nullable: false,
                auto_increment: false,
            },
        ];

        let mut txn = TxnManager::create(&wal).unwrap();
        txn.begin().unwrap();
        let rid = TableEngine::insert_row(
            &mut storage,
            &mut txn,
            &table,
            &cols,
            vec![Value::Int(1), Value::Int(10)],
        )
        .unwrap();

        let new_rids = TableEngine::update_rows_preserve_rid(
            &mut storage,
            &mut txn,
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
        let mut storage = MemoryStorage::new();
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
            },
            ColumnDef {
                table_id: 1,
                col_idx: 1,
                name: "name".into(),
                col_type: ColumnType::Text,
                nullable: false,
                auto_increment: false,
            },
        ];

        let mut txn = TxnManager::create(&wal).unwrap();
        txn.begin().unwrap();
        TableEngine::insert_row(
            &mut storage,
            &mut txn,
            &table,
            &cols,
            vec![Value::Int(7), Value::Text("alice".into())],
        )
        .unwrap();
        txn.commit().unwrap();

        let snap = txn.snapshot();
        let mask = [false, false];
        let rows =
            TableEngine::scan_table_direct(&mut storage, &table, &cols, snap, Some(&mask)).unwrap();

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, vec![Value::Null, Value::Null]);
    }
}
