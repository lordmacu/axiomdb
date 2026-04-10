// ── DROP INDEX ────────────────────────────────────────────────────────────────

/// Scans all tables in all schemas of `database` to find which table owns an
/// index named `index_name`.
///
/// Returns the first matching `TableRef` (schema = "public", name = table_name),
/// or `None` if no table in the database has an index with that name.
///
/// Used by `execute_drop_index` when no `ON table` clause is provided (4.22d).
fn find_table_for_index(
    storage: &dyn StorageEngine,
    snap: TransactionSnapshot,
    index_name: &str,
    database: &str,
) -> Result<Option<TableRef>, DbError> {
    let mut reader = CatalogReader::new(storage, snap)?;
    let tables = reader.list_tables_in_database(database, "public")?;
    for t in tables {
        let indexes = reader.list_indexes(t.id)?;
        if indexes.iter().any(|i| i.name == index_name) {
            return Ok(Some(TableRef {
                database: None,
                schema: None,
                name: t.table_name,
                alias: None,
            }));
        }
    }
    Ok(None)
}

fn execute_drop_index(
    stmt: DropIndexStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
    database: &str,
) -> Result<QueryResult, DbError> {
    let snap = txn.active_snapshot(conn_txn);

    // Resolve the target table. When ON table is omitted (4.22d), scan all
    // tables in all schemas to find one that contains an index with this name.
    let table_ref: TableRef = match stmt.table {
        Some(tr) => tr,
        None => match find_table_for_index(storage, snap.clone(), &stmt.name, database)? {
            Some(tr) => tr,
            None if stmt.if_exists => return Ok(QueryResult::Empty),
            None => {
                return Err(DbError::NotImplemented {
                    feature: format!("DROP INDEX — index '{}' not found in any table", stmt.name),
                })
            }
        },
    };

    let schema = table_ref.schema.as_deref().unwrap_or("public");

    // Capture index_id, root_page_id, clustered flag, and table_id for bump.
    let (index_id, root_page_id, clustered_primary, table_id_for_bump) = {
        let mut reader = CatalogReader::new(storage, snap)?;
        let table_def = match reader.get_table_in_database(database, schema, &table_ref.name)? {
            Some(d) => d,
            None if stmt.if_exists => return Ok(QueryResult::Empty),
            None => {
                return Err(DbError::TableNotFound {
                    name: table_ref.name.clone(),
                })
            }
        };
        let indexes = reader.list_indexes(table_def.id)?;
        match indexes.into_iter().find(|i| i.name == stmt.name) {
            Some(i) => (
                Some(i.index_id),
                Some(i.root_page_id),
                table_def.is_clustered() && i.is_primary,
                table_def.id,
            ),
            None => (None, None, false, table_def.id),
        }
    }; // reader dropped

    if clustered_primary {
        return Err(DbError::NotImplemented {
            feature: "DROP PRIMARY KEY on clustered table — Phase 39.19".into(),
        });
    }

    match index_id {
        None if stmt.if_exists => Ok(QueryResult::Empty),
        None => Err(DbError::NotImplemented {
            feature: format!("DROP INDEX — index '{}' not found", stmt.name),
        }),
        Some(id) => {
            // Delete catalog entry first.
            CatalogWriter::new(storage, txn, conn_txn)?.delete_index(id)?;
            bloom.remove(id);
            // Then free all B-Tree pages to avoid leaks.
            if let Some(root) = root_page_id {
                free_btree_pages(storage, root)?;
            }
            // Bump per-table schema_version for OID-based plan cache invalidation
            // (Phase 40.2). Cached plans that chose this index must be recompiled.
            let _ = CatalogWriter::new(storage, txn, conn_txn)?.bump_table_schema_version(table_id_for_bump);
            Ok(QueryResult::Empty)
        }
    }
}

/// Drops an index by its catalog `index_id`, without requiring the index name.
///
/// Used by `alter_drop_constraint` to remove the auto-created FK index when a
/// FK constraint is dropped.
fn execute_drop_index_by_id(
    index_id: u32,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    bloom: &crate::bloom::BloomRegistry,
) -> Result<(), DbError> {
    let snap = txn.active_snapshot(conn_txn);
    // Find the root page ID so we can free the B-Tree pages.
    let root_page_id = {
        // Scan all indexes looking for this index_id.
        // We scan axiom_indexes; CatalogReader::list_indexes requires a table_id,
        // so we use a raw catalog reader to get the TableDef first, but since we
        // only have index_id, we scan all tables. For Phase 6.5 this is acceptable
        // (index count is small). A direct get_index_by_id is deferred.
        // Scan axiom_indexes heap directly to find root by index_id (no table filter needed).
        let page_ids = axiomdb_catalog::bootstrap::CatalogBootstrap::page_ids(storage)?;
        let rows = axiomdb_storage::heap_chain::HeapChain::scan_visible_ro(
            storage,
            page_ids.indexes,
            snap,
        )?;
        let mut found_root = None;
        for (_, _, data) in rows {
            if let Ok((def, _)) = axiomdb_catalog::schema::IndexDef::from_bytes(&data) {
                if def.index_id == index_id {
                    found_root = Some(def.root_page_id);
                    break;
                }
            }
        }
        found_root
    };

    CatalogWriter::new(storage, txn, conn_txn)?.delete_index(index_id)?;
    bloom.remove(index_id);
    if let Some(root) = root_page_id {
        free_btree_pages(storage, root)?;
    }
    Ok(())
}

