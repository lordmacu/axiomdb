// ── ALTER TABLE ... REBUILD (Phase 39.19) ────────────────────────────────────

/// Converts a heap table with a PRIMARY KEY into clustered format:
/// scans rows in PK order → bulk-inserts into clustered B-tree → rebuilds
/// secondary indexes with PK bookmarks → swaps root in catalog → frees old pages.
fn alter_rebuild_to_clustered(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    resolved: &axiomdb_catalog::ResolvedTable,
    _database: &str,
    _schema: &str,
) -> Result<QueryResult, DbError> {
    use axiomdb_storage::{clustered_tree, page::PageType};

    // ── Validate preconditions ───────────────────────────────────────────
    if resolved.def.is_clustered() {
        return Err(DbError::Other(format!(
            "table '{}' is already clustered",
            resolved.def.table_name
        )));
    }

    let pk_idx = resolved
        .indexes
        .iter()
        .find(|i| i.is_primary)
        .ok_or_else(|| DbError::Other(format!(
            "ALTER TABLE REBUILD requires a PRIMARY KEY on '{}'",
            resolved.def.table_name
        )))?;

    let pk_col_positions: Vec<usize> = pk_idx
        .columns
        .iter()
        .map(|c| c.col_idx as usize)
        .collect();

    let columns = &resolved.columns;
    let col_types: Vec<axiomdb_types::DataType> = columns
        .iter()
        .map(|c| crate::table::column_type_to_data_type(c.col_type))
        .collect();

    let _snap = txn.active_snapshot(conn_txn);
    let txn_id = conn_txn.txn_id;
    let old_root = resolved.def.root_page_id;

    // ── Phase 1: Scan heap rows in PK order ──────────────────────────────
    // Use PK B-tree to iterate in key order → batch heap read → decode.
    let pairs = axiomdb_index::BTree::range_in(storage, pk_idx.root_page_id, None, None)?;
    let rids: Vec<axiomdb_core::RecordId> = pairs.into_iter().map(|(rid, _)| rid).collect();
    let batch_rows = crate::table::TableEngine::read_rows_batch(storage, columns, &rids)?;

    let mut sorted_rows: Vec<Vec<axiomdb_types::Value>> = Vec::with_capacity(rids.len());
    for (idx, values) in batch_rows.into_iter().enumerate() {
        match values {
            Some(values) => sorted_rows.push(values),
            None => {
                return Err(DbError::IndexIntegrityFailure {
                    table: format!("{}.{}", resolved.def.schema_name, resolved.def.table_name),
                    index: pk_idx.name.clone(),
                    reason: format!(
                        "PRIMARY KEY index points at dead heap slot {:?} during ALTER TABLE ... REBUILD",
                        rids[idx]
                    ),
                })
            }
        }
    }

    // ── Phase 2: Bulk-insert into new clustered B-tree ───────────────────
    // Rows arrive in PK order → inserts are append-only → no rebalancing.
    let mut new_root_pid: Option<u64> = None;

    for values in &sorted_rows {
        let pk_values: Vec<axiomdb_types::Value> = pk_col_positions
            .iter()
            .map(|&pos| values[pos].clone())
            .collect();
        let pk_key = crate::key_encoding::encode_index_key(&pk_values)?;
        let row_data = axiomdb_types::codec::encode_row(values, &col_types)?;
        let header = axiomdb_storage::heap::RowHeader {
            txn_id_created: txn_id,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };

        new_root_pid = Some(clustered_tree::insert(
            storage,
            new_root_pid,
            &pk_key,
            &header,
            &row_data,
        )?);
    }

    let clustered_root = match new_root_pid {
        Some(root) => root,
        None => alloc_empty_clustered_root(storage)?,
    };

    // ── Phase 3: Rebuild secondary indexes with PK bookmarks ─────────────
    let secondary_indexes: Vec<&axiomdb_catalog::IndexDef> = resolved
        .indexes
        .iter()
        .filter(|i| !i.is_primary && !i.columns.is_empty())
        .collect();

    let mut new_sec_roots: Vec<(u32, u64)> = Vec::new(); // (index_id, new_root)
    let rebuild_result = (|| -> Result<QueryResult, DbError> {
        for sec_idx in &secondary_indexes {
            let layout =
                crate::clustered_secondary::ClusteredSecondaryLayout::derive(sec_idx, pk_idx)?;

            // Allocate fresh B-tree root for the secondary index.
            let new_sec_root_pid = storage.alloc_page(PageType::Index)?;
            {
                let mut page = axiomdb_storage::Page::new(PageType::Index, new_sec_root_pid);
                let leaf = axiomdb_index::page_layout::cast_leaf_mut(&mut page);
                leaf.is_leaf = 1;
                leaf.set_num_keys(0);
                leaf.set_next_leaf(axiomdb_index::page_layout::NULL_PAGE);
                page.update_checksum();
                storage.write_page(new_sec_root_pid, &page)?;
            }

            let sec_root_atomic = std::sync::atomic::AtomicU64::new(new_sec_root_pid);

            // Scan clustered tree and insert secondary entries.
            for values in &sorted_rows {
                layout.insert_row(storage, &sec_root_atomic, values)?;
            }

            new_sec_roots.push((
                sec_idx.index_id,
                sec_root_atomic.load(std::sync::atomic::Ordering::Acquire),
            ));
        }

        let mut old_pages = collect_heap_chain_pages(storage, old_root)?;
        old_pages.extend(collect_btree_pages(storage, pk_idx.root_page_id)?);
        for sec_idx in &secondary_indexes {
            old_pages.extend(collect_btree_pages(storage, sec_idx.root_page_id)?);
        }
        old_pages.sort_unstable();
        old_pages.dedup();

        storage.flush()?;

        // ── Phase 4: Atomic catalog swap + deferred reclamation ──────────
        {
            let mut writer = axiomdb_catalog::CatalogWriter::new(storage, txn, conn_txn)?;

            // Update table: root → clustered root, layout → Clustered.
            writer.update_table_root(resolved.def.id, clustered_root)?;
            writer.update_storage_layout(
                resolved.def.id,
                axiomdb_catalog::TableStorageLayout::Clustered,
            )?;

            // Update PK index root to match clustered root.
            writer.update_index_root(pk_idx.index_id, clustered_root)?;

            // Update secondary index roots.
            for (idx_id, new_root) in &new_sec_roots {
                writer.update_index_root(*idx_id, *new_root)?;
            }
        }

        txn.defer_free_pages(conn_txn, old_pages);

        Ok(QueryResult::Affected {
            count: sorted_rows.len() as u64,
            last_insert_id: None,
        })
    })();

    if let Err(err) = rebuild_result {
        cleanup_rebuild_artifacts(
            storage,
            Some(clustered_root),
            new_sec_roots.iter().map(|(_, root)| *root),
        );
        return Err(err);
    }

    rebuild_result
}

fn alloc_empty_clustered_root(storage: &dyn StorageEngine) -> Result<u64, DbError> {
    let pid = storage.alloc_page(PageType::ClusteredLeaf)?;
    let mut page = axiomdb_storage::Page::new(PageType::ClusteredLeaf, pid);
    axiomdb_storage::clustered_leaf::init_clustered_leaf(&mut page);
    page.update_checksum();
    storage.write_page(pid, &page)?;
    Ok(pid)
}

fn cleanup_rebuild_artifacts<I>(
    storage: &dyn StorageEngine,
    clustered_root: Option<u64>,
    secondary_roots: I,
) where
    I: IntoIterator<Item = u64>,
{
    if let Some(root) = clustered_root {
        let _ = free_clustered_tree_pages(storage, root);
    }
    for root in secondary_roots {
        let _ = free_btree_pages(storage, root);
    }
}

fn free_clustered_tree_pages(
    storage: &dyn StorageEngine,
    root_pid: u64,
) -> Result<(), DbError> {
    let mut stack = vec![root_pid];
    let mut visited = std::collections::HashSet::new();

    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }

        let page = storage.read_page(pid)?;
        let page_type = PageType::try_from(page.header().page_type).map_err(|err| {
            DbError::BTreeCorrupted {
                msg: format!("clustered rebuild cleanup saw invalid page type at {pid}: {err}"),
            }
        })?;

        match page_type {
            PageType::ClusteredLeaf => {
                let n = axiomdb_storage::clustered_leaf::num_cells(&page);
                for idx in 0..n {
                    let cell = axiomdb_storage::clustered_leaf::read_cell(&page, idx)?;
                    if let Some(first_overflow) = cell.overflow_first_page {
                        axiomdb_storage::clustered_overflow::free_chain(storage, first_overflow)?;
                    }
                }
                storage.free_page(pid)?;
            }
            PageType::ClusteredInternal => {
                let n = axiomdb_storage::clustered_internal::num_cells(&page);
                for idx in 0..=n {
                    let child = axiomdb_storage::clustered_internal::child_at(&page, idx)?;
                    if child != axiomdb_storage::clustered_internal::NULL_PAGE {
                        stack.push(child);
                    }
                }
                storage.free_page(pid)?;
            }
            other => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!(
                        "clustered rebuild cleanup expected clustered page at {pid}, found {other:?}"
                    ),
                })
            }
        }
    }

    Ok(())
}
