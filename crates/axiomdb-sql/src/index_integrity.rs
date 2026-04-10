//! Startup-time index integrity verification and repair (Phase 6.15).
//!
//! Checks every catalog-visible index against heap-visible rows and rebuilds
//! divergent-but-readable indexes before the database accepts traffic.

use std::sync::atomic::{AtomicU64, Ordering};

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
    clustered_secondary::ClusteredSecondaryLayout,
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
) -> Result<IndexIntegrityReport, DbError> {
    let snapshot = txn.snapshot();
    let tables = list_visible_tables(storage, snapshot.clone())?;
    let mut report = IndexIntegrityReport::default();
    let mut pending = Vec::new();

    for table_def in &tables {
        report.tables_checked += 1;
        let (col_defs, indexes) = {
            let mut reader = CatalogReader::new(storage, snapshot.clone())?;
            (
                reader.list_columns(table_def.id)?,
                reader.list_indexes(table_def.id)?,
            )
        };

        if indexes.is_empty() {
            continue;
        }

        if table_def.is_clustered() {
            // For clustered tables, verify secondary indexes only.
            // The primary "index" IS the clustered B-tree (data == index); no separate
            // primary index structure can diverge from the data itself.
            let secondary: Vec<&IndexDef> = indexes
                .iter()
                .filter(|i| !i.is_primary && !i.columns.is_empty())
                .collect();
            if secondary.is_empty() {
                continue;
            }

            let primary_idx = match indexes
                .iter()
                .find(|i| i.is_primary && !i.columns.is_empty())
            {
                Some(idx) => idx,
                None => continue, // no PK metadata — skip
            };

            let rows = crate::table::scan_clustered_table(
                storage,
                table_def,
                &col_defs,
                snapshot.clone(),
            )?;
            let compiled_preds = compile_index_predicates(&indexes, &col_defs)?;

            for idx in secondary {
                // Find the compiled predicate for this index (by position in the full list).
                let idx_pos = indexes.iter().position(|i| i.index_id == idx.index_id);
                let compiled_pred = idx_pos
                    .and_then(|p| compiled_preds.get(p))
                    .and_then(|p| p.as_ref());

                report.indexes_checked += 1;

                let layout = match ClusteredSecondaryLayout::derive(idx, primary_idx) {
                    Ok(l) => l,
                    Err(_) => continue,
                };

                let expected =
                    expected_clustered_secondary_entries(&layout, idx, compiled_pred, &rows)?;
                let actual = actual_entries_for_index(storage, table_def, idx)?;
                if actual == expected {
                    continue;
                }

                let new_root = match build_clustered_secondary_from_scan(
                    storage,
                    &layout,
                    idx,
                    compiled_pred,
                    &rows,
                ) {
                    Ok(r) => r,
                    Err(err) => {
                        cleanup_pending_new_roots(storage, &pending);
                        return Err(err);
                    }
                };
                let old_pages = match collect_btree_pages(storage, idx.root_page_id) {
                    Ok(p) => p,
                    Err(err) => {
                        let _ = free_btree_pages(storage, new_root);
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
                    new_root,
                    old_pages,
                });
            }
        } else {
            // Heap table: verify all indexes.
            let rows =
                TableEngine::scan_table(storage, table_def, &col_defs, snapshot.clone(), None)?;
            let compiled_preds = compile_index_predicates(&indexes, &col_defs)?;

            for (idx, compiled_pred) in indexes.iter().zip(compiled_preds.iter()) {
                report.indexes_checked += 1;
                let expected = expected_entries_for_index(idx, compiled_pred.as_ref(), &rows)?;
                let actual = actual_entries_for_index(storage, table_def, idx)?;
                if actual == expected {
                    continue;
                }

                let build = match build_index_root_from_heap(
                    storage,
                    table_def,
                    &col_defs,
                    idx,
                    snapshot.clone(),
                ) {
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
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    pending: &[PendingRebuild],
) -> Result<(), DbError> {
    // The rebuilt B+Tree pages are written directly into the mmap-backed data
    // file, not through WAL. Flush them before the catalog root swap commits so
    // WAL recovery never points at pages that were only resident in memory.
    if let Err(err) = storage.flush() {
        cleanup_pending_new_roots(storage, pending);
        return Err(err);
    }

    let mut conn_txn = match txn.begin() {
        Ok(ct) => ct,
        Err(err) => {
            cleanup_pending_new_roots(storage, pending);
            return Err(err);
        }
    };
    let txn_id = conn_txn.txn_id;

    let apply_result = (|| -> Result<(), DbError> {
        let mut writer = CatalogWriter::new(storage, txn, &mut conn_txn)?;
        let mut old_pages_to_free = Vec::new();
        for rebuild in pending {
            writer.update_index_root(rebuild.index_id, rebuild.new_root)?;
            old_pages_to_free.extend_from_slice(&rebuild.old_pages);
        }
        old_pages_to_free.sort_unstable();
        old_pages_to_free.dedup();
        txn.defer_free_pages(&mut conn_txn, old_pages_to_free);
        Ok(())
    })();

    if let Err(err) = apply_result {
        let _ = txn.rollback(conn_txn, storage);
        cleanup_pending_new_roots(storage, pending);
        return Err(err);
    }

    txn.commit(conn_txn)?;
    txn.release_immediate_committed_frees(storage, txn_id)?;
    txn.drain_committed_page_batches(storage)?;
    Ok(())
}

fn cleanup_pending_new_roots(storage: &dyn StorageEngine, pending: &[PendingRebuild]) {
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
    storage: &dyn StorageEngine,
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

/// Computes expected B-Tree entries for a clustered secondary index from a
/// full row scan, using `ClusteredSecondaryLayout::entry_from_row` for key
/// encoding.  NULL secondary columns and predicate-excluded rows are skipped.
fn expected_clustered_secondary_entries(
    layout: &ClusteredSecondaryLayout,
    idx: &IndexDef,
    compiled_pred: Option<&crate::expr::Expr>,
    rows: &[(RecordId, Vec<Value>)],
) -> Result<Vec<IndexEntry>, DbError> {
    const DUMMY_RID: RecordId = RecordId {
        page_id: 0,
        slot_id: 0,
    };
    let mut entries = Vec::new();
    for (_, row) in rows {
        // Apply partial index predicate (returns None when predicate excludes row).
        if index_key_values_if_indexed(idx, row, compiled_pred)?.is_none() {
            continue;
        }
        // entry_from_row returns None when any secondary column is NULL.
        let Some(entry) = layout.entry_from_row(row)? else {
            continue;
        };
        entries.push(IndexEntry {
            key: entry.physical_key,
            rid: DUMMY_RID,
        });
    }
    sort_entries(&mut entries);
    Ok(entries)
}

/// Builds a new clustered secondary index B-tree from a full row scan.
///
/// Allocates a fresh empty B-tree root, then calls
/// `ClusteredSecondaryLayout::insert_row` for every row that passes the
/// partial-index predicate (if any) and has non-NULL secondary columns.
fn build_clustered_secondary_from_scan(
    storage: &dyn StorageEngine,
    layout: &ClusteredSecondaryLayout,
    idx: &IndexDef,
    compiled_pred: Option<&crate::expr::Expr>,
    rows: &[(RecordId, Vec<Value>)],
) -> Result<u64, DbError> {
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
    use axiomdb_storage::{Page, PageType};

    // Allocate empty B-Tree leaf root.
    let root_pid = storage.alloc_page(PageType::Index)?;
    {
        let mut page = Page::new(PageType::Index, root_pid);
        let leaf = cast_leaf_mut(&mut page);
        leaf.is_leaf = 1;
        leaf.set_num_keys(0);
        leaf.set_next_leaf(NULL_PAGE);
        page.update_checksum();
        storage.write_page(root_pid, &page)?;
    }

    let root_atomic = AtomicU64::new(root_pid);
    for (_, row) in rows {
        if index_key_values_if_indexed(idx, row, compiled_pred)?.is_none() {
            continue; // predicate excludes row
        }
        layout.insert_row(storage, &root_atomic, row)?;
    }
    Ok(root_atomic.load(Ordering::Acquire))
}
