//! MVCC Vacuum — removes dead rows and dead index entries (Phase 7.11).
//!
//! After MVCC lazy index deletion (Phase 7.3b), deleted rows and dead index
//! entries accumulate. `VACUUM` physically removes them:
//!
//! - **Heap:** slots where `txn_id_deleted < oldest_safe_txn` are zeroed via
//!   `mark_slot_dead()`, making them invisible to `read_tuple()`.
//! - **Indexes:** entries pointing to dead heap slots are deleted from the B-Tree.
//!
//! Only non-unique, non-FK secondary indexes are vacuumed. Unique/PK/FK indexes
//! already have their entries deleted immediately during DML (Phase 7.3b).

use std::sync::atomic::AtomicU64;

use axiomdb_catalog::{CatalogReader, IndexDef, SchemaResolver};
use axiomdb_core::{error::DbError, TransactionSnapshot};
use axiomdb_index::BTree;
use axiomdb_storage::{
    heap::{mark_slot_dead, num_slots, read_slot, read_tuple_header},
    heap_chain::{chain_next_page, HeapChain},
    Page, StorageEngine,
};
use axiomdb_types::{DataType, Value};
use axiomdb_wal::TxnManager;

use crate::ast::VacuumStmt;
use crate::result::{ColumnMeta, QueryResult};
use crate::session::SessionContext;

// ── Public result ────────────────────────────────────────────────────────────

/// Statistics returned by `vacuum_table`.
#[derive(Debug)]
pub struct VacuumTableResult {
    pub table_name: String,
    pub dead_rows_removed: u64,
    pub dead_index_entries_removed: u64,
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Executes `VACUUM [table_name]`.
///
/// If `stmt.table` is `None`, vacuums all tables in the current database.
/// Returns one `VacuumTableResult` per table vacuumed.
pub fn execute_vacuum(
    stmt: VacuumStmt,
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot().unwrap_or_else(|_| txn.snapshot());
    // Under RwLock: no concurrent readers, all committed deletes are safe.
    let oldest_safe_txn = txn.max_committed() + 1;

    let results = if let Some(ref table_ref) = stmt.table {
        let db = ctx.effective_database().to_string();
        let mut resolver = SchemaResolver::new(storage, snap, &db, "public")?;
        let resolved = resolver.resolve_table(table_ref.schema.as_deref(), &table_ref.name)?;
        let r = vacuum_one_table(
            &resolved.def,
            &resolved.indexes,
            storage,
            snap,
            oldest_safe_txn,
            bloom,
        )?;
        vec![r]
    } else {
        // Vacuum all tables in current database.
        let db = ctx.effective_database().to_string();
        let tables = {
            let mut reader = CatalogReader::new(storage, snap)?;
            reader.list_tables_in_database(&db, "public")?
        };
        let mut results = Vec::new();
        for table_def in &tables {
            let indexes = {
                let mut reader = CatalogReader::new(storage, snap)?;
                reader.list_indexes(table_def.id)?
            };
            let r = vacuum_one_table(table_def, &indexes, storage, snap, oldest_safe_txn, bloom)?;
            results.push(r);
        }
        results
    };

    // Format as QueryResult::Rows.
    let columns = vec![
        ColumnMeta {
            name: "table".into(),
            data_type: DataType::Text,
            nullable: false,
            table_name: None,
        },
        ColumnMeta {
            name: "dead_rows_removed".into(),
            data_type: DataType::Int,
            nullable: false,
            table_name: None,
        },
        ColumnMeta {
            name: "dead_index_entries_removed".into(),
            data_type: DataType::Int,
            nullable: false,
            table_name: None,
        },
    ];
    let rows: Vec<Vec<Value>> = results
        .iter()
        .map(|r| {
            vec![
                Value::Text(r.table_name.clone()),
                Value::Int(r.dead_rows_removed as i32),
                Value::Int(r.dead_index_entries_removed as i32),
            ]
        })
        .collect();
    Ok(QueryResult::Rows { columns, rows })
}

// ── Per-table vacuum ─────────────────────────────────────────────────────────

fn vacuum_one_table(
    table_def: &axiomdb_catalog::TableDef,
    indexes: &[IndexDef],
    storage: &mut dyn StorageEngine,
    snap: TransactionSnapshot,
    oldest_safe_txn: u64,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<VacuumTableResult, DbError> {
    // 1. Heap vacuum: walk the heap chain, mark dead slots.
    let dead_rows = vacuum_heap_chain(storage, table_def.data_root_page_id, oldest_safe_txn)?;

    // 2. Index vacuum: clean dead entries from ALL indexes (PostgreSQL model).
    // Since DELETE/UPDATE no longer remove index entries for any index type,
    // VACUUM must clean dead entries from PK, UNIQUE, FK, and non-unique alike.
    let mut dead_index_entries = 0u64;
    for idx in indexes {
        if idx.columns.is_empty() {
            continue;
        }
        dead_index_entries += vacuum_index(storage, idx, snap, bloom)?;
    }

    Ok(VacuumTableResult {
        table_name: table_def.table_name.clone(),
        dead_rows_removed: dead_rows,
        dead_index_entries_removed: dead_index_entries,
    })
}

// ── Heap vacuum ──────────────────────────────────────────────────────────────

/// Walks the heap chain and physically kills slots where
/// `txn_id_deleted != 0 && txn_id_deleted < oldest_safe_txn`.
fn vacuum_heap_chain(
    storage: &mut dyn StorageEngine,
    root_page_id: u64,
    oldest_safe_txn: u64,
) -> Result<u64, DbError> {
    let mut dead_count = 0u64;
    let mut page_id = root_page_id;

    while page_id != 0 {
        let raw = *storage.read_page(page_id)?.as_bytes();
        let mut page = Page::from_bytes(raw)?;
        let n = num_slots(&page);
        let mut page_modified = false;

        for slot_id in 0..n {
            let entry = read_slot(&page, slot_id);
            if entry.is_dead() {
                continue; // already vacuumed or rolled back
            }
            // read_tuple_header returns the txn_id_deleted field (None = alive).
            let txn_id_deleted = read_tuple_header(&page, slot_id)?;
            if let Some(del_txn) = txn_id_deleted {
                if del_txn != 0 && del_txn < oldest_safe_txn {
                    mark_slot_dead(&mut page, slot_id)?;
                    dead_count += 1;
                    page_modified = true;
                }
            }
        }

        // Only write back pages that were actually modified.
        let next = chain_next_page(&page);
        if page_modified {
            page.update_checksum();
            storage.write_page(page_id, &page)?;
        }

        page_id = next;
    }

    Ok(dead_count)
}

// ── Index vacuum ─────────────────────────────────────────────────────────────

/// Scans all entries in a non-unique secondary index and removes those
/// pointing to dead heap slots.
fn vacuum_index(
    storage: &mut dyn StorageEngine,
    index: &IndexDef,
    snap: TransactionSnapshot,
    bloom: &mut crate::bloom::BloomRegistry,
) -> Result<u64, DbError> {
    let all_entries = BTree::range_in(storage, index.root_page_id, None, None)?;
    if all_entries.is_empty() {
        return Ok(0);
    }

    // Collect keys of dead entries.
    let mut dead_keys: Vec<Vec<u8>> = Vec::new();
    for (rid, key_bytes) in &all_entries {
        if !HeapChain::is_slot_visible(storage, rid.page_id, rid.slot_id, snap)? {
            dead_keys.push(key_bytes.clone());
        }
    }

    if dead_keys.is_empty() {
        return Ok(0);
    }

    let count = dead_keys.len() as u64;
    dead_keys.sort_unstable();
    let root_pid = AtomicU64::new(index.root_page_id);
    BTree::delete_many_in(storage, &root_pid, &dead_keys)?;

    // Mark bloom as dirty so it's rebuilt on next use.
    bloom.mark_dirty(index.index_id);

    Ok(count)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use axiomdb_storage::{
        heap::{insert_tuple, read_tuple},
        page::PageType,
        MemoryStorage,
    };

    fn make_page_with_rows(storage: &mut MemoryStorage, n: u16, txn_id: u64) -> (u64, Vec<u16>) {
        let page_id = storage.alloc_page(PageType::Data).unwrap();
        let raw = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(raw).unwrap();
        let mut slots = Vec::new();
        for i in 0..n {
            let data = format!("row-{i}");
            let sid = insert_tuple(&mut page, data.as_bytes(), txn_id).unwrap();
            slots.push(sid);
        }
        page.update_checksum();
        storage.write_page(page_id, &page).unwrap();
        (page_id, slots)
    }

    fn mark_deleted_in_page(storage: &mut MemoryStorage, page_id: u64, slot_id: u16, txn_id: u64) {
        let raw = *storage.read_page(page_id).unwrap().as_bytes();
        let mut page = Page::from_bytes(raw).unwrap();
        axiomdb_storage::heap::delete_tuple(&mut page, slot_id, txn_id).unwrap();
        storage.write_page(page_id, &page).unwrap();
    }

    #[test]
    fn test_vacuum_heap_marks_dead_slots() {
        let mut storage = MemoryStorage::new();
        let (page_id, slots) = make_page_with_rows(&mut storage, 5, 1);

        // Delete 3 rows with txn_id=2.
        for &sid in &slots[0..3] {
            mark_deleted_in_page(&mut storage, page_id, sid, 2);
        }

        // Vacuum with oldest_safe_txn=3 (txn 2 is committed and safe).
        let removed = vacuum_heap_chain(&mut storage, page_id, 3).unwrap();
        assert_eq!(removed, 3);

        // Verify: slots 0-2 are dead, slots 3-4 are alive.
        let raw = *storage.read_page(page_id).unwrap().as_bytes();
        let page = Page::from_bytes(raw).unwrap();
        for &sid in &slots[0..3] {
            assert!(
                read_tuple(&page, sid).unwrap().is_none(),
                "slot {sid} should be dead"
            );
        }
        for &sid in &slots[3..5] {
            assert!(
                read_tuple(&page, sid).unwrap().is_some(),
                "slot {sid} should be alive"
            );
        }
    }

    #[test]
    fn test_vacuum_heap_preserves_live_rows() {
        let mut storage = MemoryStorage::new();
        let (page_id, _) = make_page_with_rows(&mut storage, 5, 1);

        let removed = vacuum_heap_chain(&mut storage, page_id, 100).unwrap();
        assert_eq!(removed, 0);
    }

    #[test]
    fn test_vacuum_heap_preserves_recent_deletes() {
        let mut storage = MemoryStorage::new();
        let (page_id, slots) = make_page_with_rows(&mut storage, 3, 1);

        // Delete with txn_id=10.
        mark_deleted_in_page(&mut storage, page_id, slots[0], 10);

        // Vacuum with oldest_safe_txn=5 → txn 10 is NOT safe yet.
        let removed = vacuum_heap_chain(&mut storage, page_id, 5).unwrap();
        assert_eq!(removed, 0, "recently deleted row should be preserved");

        // Vacuum with oldest_safe_txn=11 → now safe.
        let removed = vacuum_heap_chain(&mut storage, page_id, 11).unwrap();
        assert_eq!(removed, 1);
    }
}
