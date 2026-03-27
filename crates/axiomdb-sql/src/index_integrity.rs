//! Startup-time index integrity verification and repair (Phase 6.15).
//!
//! Checks every catalog-visible index against heap-visible rows and rebuilds
//! divergent-but-readable indexes before the database accepts traffic.

use axiomdb_catalog::{
    bootstrap::CatalogBootstrap,
    schema::{IndexDef, TableDef},
    CatalogReader, CatalogWriter,
};
use axiomdb_core::{error::DbError, RecordId, TransactionSnapshot};
use axiomdb_index::BTree;
use axiomdb_storage::{HeapChain, StorageEngine};
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

use crate::{
    executor::{build_index_root_from_heap, collect_btree_pages, free_btree_pages},
    index_maintenance::{encode_index_entry_key, index_key_values_if_indexed},
    partial_index::compile_index_predicates,
    TableEngine,
};

#[derive(Debug, Default, Clone)]
pub struct IndexIntegrityReport {
    pub tables_checked: usize,
    pub indexes_checked: usize,
    pub rebuilt_indexes: Vec<RebuiltIndex>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuiltIndex {
    pub table_name: String,
    pub index_name: String,
    pub index_id: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct IndexEntry {
    key: Vec<u8>,
    rid: RecordId,
}

#[derive(Debug)]
struct PendingRebuild {
    table_name: String,
    index_name: String,
    index_id: u32,
    old_root: u64,
    new_root: u64,
    old_pages: Vec<u64>,
}

pub fn verify_and_repair_indexes_on_open(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
) -> Result<IndexIntegrityReport, DbError> {
    let snapshot = txn.snapshot();
    let tables = list_visible_tables(storage, snapshot)?;
    let mut report = IndexIntegrityReport {
        tables_checked: tables.len(),
        ..IndexIntegrityReport::default()
    };
    let mut pending = Vec::new();

    for table_def in &tables {
        let (col_defs, indexes) = {
            let mut reader = CatalogReader::new(storage, snapshot)?;
            (
                reader.list_columns(table_def.id)?,
                reader.list_indexes(table_def.id)?,
            )
        };

        if indexes.is_empty() {
            continue;
        }

        let rows = TableEngine::scan_table(storage, table_def, &col_defs, snapshot, None)?;
        let compiled_preds = compile_index_predicates(&indexes, &col_defs)?;

        for (idx, compiled_pred) in indexes.iter().zip(compiled_preds.iter()) {
            report.indexes_checked += 1;
            let expected = expected_entries_for_index(idx, compiled_pred.as_ref(), &rows)?;
            let actual = actual_entries_for_index(storage, table_def, idx)?;
            if actual == expected {
                continue;
            }

            let build =
                match build_index_root_from_heap(storage, table_def, &col_defs, idx, snapshot) {
                    Ok(build) => build,
                    Err(err) => {
                        cleanup_pending_new_roots(storage, &pending);
                        return Err(err);
                    }
                };
            let old_pages = match collect_btree_pages(storage, idx.root_page_id) {
                Ok(old_pages) => old_pages,
                Err(err) => {
                    let _ = free_btree_pages(storage, build.root_page_id);
                    cleanup_pending_new_roots(storage, &pending);
                    return Err(DbError::IndexIntegrityFailure {
                        table: format!("{}.{}", table_def.schema_name, table_def.table_name),
                        index: idx.name.clone(),
                        reason: err.to_string(),
                    });
                }
            };
            pending.push(PendingRebuild {
                table_name: table_def.table_name.clone(),
                index_name: idx.name.clone(),
                index_id: idx.index_id,
                old_root: idx.root_page_id,
                new_root: build.root_page_id,
                old_pages,
            });
        }
    }

    if pending.is_empty() {
        return Ok(report);
    }

    apply_pending_rebuilds(storage, txn, &pending)?;
    report.rebuilt_indexes = pending
        .into_iter()
        .map(|p| RebuiltIndex {
            table_name: p.table_name,
            index_name: p.index_name,
            index_id: p.index_id,
        })
        .collect();
    Ok(report)
}

fn apply_pending_rebuilds(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    pending: &[PendingRebuild],
) -> Result<(), DbError> {
    // The rebuilt B+Tree pages are written directly into the mmap-backed data
    // file, not through WAL. Flush them before the catalog root swap commits so
    // WAL recovery never points at pages that were only resident in memory.
    if let Err(err) = storage.flush() {
        cleanup_pending_new_roots(storage, pending);
        return Err(err);
    }

    let txn_id = match txn.begin() {
        Ok(id) => id,
        Err(err) => {
            cleanup_pending_new_roots(storage, pending);
            return Err(err);
        }
    };

    let apply_result = (|| -> Result<(), DbError> {
        let mut writer = CatalogWriter::new(storage, txn)?;
        let mut old_pages_to_free = Vec::new();
        for rebuild in pending {
            writer.update_index_root(rebuild.index_id, rebuild.new_root)?;
            old_pages_to_free.extend_from_slice(&rebuild.old_pages);
        }
        old_pages_to_free.sort_unstable();
        old_pages_to_free.dedup();
        txn.defer_free_pages(old_pages_to_free)?;
        Ok(())
    })();

    if let Err(err) = apply_result {
        let _ = txn.rollback(storage);
        cleanup_pending_new_roots(storage, pending);
        return Err(err);
    }

    txn.commit()?;
    txn.release_immediate_committed_frees(storage, txn_id)?;
    Ok(())
}

fn cleanup_pending_new_roots(storage: &mut dyn StorageEngine, pending: &[PendingRebuild]) {
    for rebuild in pending {
        if rebuild.new_root != rebuild.old_root {
            let _ = free_btree_pages(storage, rebuild.new_root);
        }
    }
}

fn list_visible_tables(
    storage: &dyn StorageEngine,
    snapshot: TransactionSnapshot,
) -> Result<Vec<TableDef>, DbError> {
    let page_ids = CatalogBootstrap::page_ids(storage)?;
    let rows = HeapChain::scan_visible_ro(storage, page_ids.tables, snapshot)?;
    let mut tables = Vec::with_capacity(rows.len());
    for (_, _, data) in rows {
        let (def, _) = TableDef::from_bytes(&data)?;
        tables.push(def);
    }
    tables.sort_by(|a, b| {
        a.schema_name
            .cmp(&b.schema_name)
            .then_with(|| a.table_name.cmp(&b.table_name))
            .then_with(|| a.id.cmp(&b.id))
    });
    Ok(tables)
}

fn expected_entries_for_index(
    idx: &IndexDef,
    compiled_pred: Option<&crate::expr::Expr>,
    rows: &[(RecordId, Vec<Value>)],
) -> Result<Vec<IndexEntry>, DbError> {
    let mut entries = Vec::new();
    for (rid, row) in rows {
        let Some(key_vals) = index_key_values_if_indexed(idx, row, compiled_pred)? else {
            continue;
        };

        let key = match encode_index_entry_key(idx, &key_vals, *rid) {
            Ok(key) => key,
            Err(DbError::IndexKeyTooLong { .. }) => continue,
            Err(err) => return Err(err),
        };
        entries.push(IndexEntry { key, rid: *rid });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

fn actual_entries_for_index(
    storage: &mut dyn StorageEngine,
    table_def: &TableDef,
    idx: &IndexDef,
) -> Result<Vec<IndexEntry>, DbError> {
    let rows = BTree::range_in(storage, idx.root_page_id, None, None).map_err(|err| {
        DbError::IndexIntegrityFailure {
            table: format!("{}.{}", table_def.schema_name, table_def.table_name),
            index: idx.name.clone(),
            reason: err.to_string(),
        }
    })?;
    let mut entries = rows
        .into_iter()
        .map(|(rid, key)| IndexEntry { key, rid })
        .collect::<Vec<_>>();
    sort_entries(&mut entries);
    Ok(entries)
}

fn sort_entries(entries: &mut [IndexEntry]) {
    entries.sort_by(|a, b| {
        a.key
            .cmp(&b.key)
            .then_with(|| a.rid.page_id.cmp(&b.rid.page_id))
            .then_with(|| a.rid.slot_id.cmp(&b.rid.slot_id))
    });
}
