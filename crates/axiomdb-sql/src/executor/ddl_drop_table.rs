// ── DROP TABLE ────────────────────────────────────────────────────────────────

fn execute_drop_table(
    stmt: DropTableStmt,
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    search_path: Option<&[String]>,
    database: &str,
) -> Result<QueryResult, DbError> {
    for table_ref in stmt.tables {
        let table_db = table_ref.database.as_deref().unwrap_or(database);
        let snap = txn.active_snapshot(conn_txn);

        let table_id = if let Some(schema) = table_ref.schema.as_deref() {
            let mut reader = CatalogReader::new(storage, snap)?;
            match reader.get_table_in_database(table_db, schema, &table_ref.name)? {
                Some(def) => def.id,
                None if stmt.if_exists => continue,
                None => {
                    return Err(DbError::TableNotFound {
                        name: table_ref.name.clone(),
                    })
                }
            }
        } else if let Some(search_path) = search_path {
            let mut found = None;
            for schema in search_path {
                let mut reader = CatalogReader::new(storage, snap.clone())?;
                if let Some(def) = reader.get_table_in_database(table_db, schema, &table_ref.name)? {
                    found = Some(def.id);
                    break;
                }
            }
            match found {
                Some(id) => id,
                None if stmt.if_exists => continue,
                None => {
                    return Err(DbError::TableNotFound {
                        name: table_ref.name.clone(),
                    })
                }
            }
        } else {
            let mut reader = CatalogReader::new(storage, snap)?;
            match reader.get_table_in_database(table_db, "public", &table_ref.name)? {
                Some(def) => def.id,
                None if stmt.if_exists => continue,
                None => {
                    return Err(DbError::TableNotFound {
                        name: table_ref.name.clone(),
                    })
                }
            }
        }; // reader dropped — immutable borrow released

        // Bump schema_version before dropping so cross-connection plan caches
        // that hold a dep on this table_id see a version mismatch on next lookup
        // (belt-and-suspenders: `is_stale()` also detects the table being gone).
        // Ignore errors — bump is advisory; the drop itself is authoritative.
        let _ = CatalogWriter::new(storage, txn, conn_txn)?.bump_table_schema_version(table_id);

        drop_table_fully(storage, txn, conn_txn, table_id)?;
    }

    Ok(QueryResult::Empty)
}

fn drop_table_fully(
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut axiomdb_wal::ConnectionTxn,
    table_id: u32,
) -> Result<(), DbError> {
    use std::collections::HashSet;

    let snap = txn.active_snapshot(conn_txn);
    let (constraints, child_fks, parent_fks) = {
        let mut reader = CatalogReader::new(storage, snap)?;
        (
            reader.list_constraints(table_id)?,
            reader.list_fk_constraints(table_id)?,
            reader.list_fk_constraints_referencing(table_id)?,
        )
    };

    let mut seen_fk_ids = HashSet::new();
    let noop_bloom = crate::bloom::BloomRegistry::new();
    for fk in child_fks.into_iter().chain(parent_fks) {
        if !seen_fk_ids.insert(fk.fk_id) {
            continue;
        }
        CatalogWriter::new(storage, txn, conn_txn)?.drop_foreign_key(fk.fk_id)?;
        if fk.fk_index_id != 0 {
            let _ = execute_drop_index_by_id(fk.fk_index_id, storage, txn, conn_txn, &noop_bloom);
        }
    }

    for constraint in constraints {
        CatalogWriter::new(storage, txn, conn_txn)?.drop_constraint(constraint.constraint_id)?;
    }
    CatalogWriter::new(storage, txn, conn_txn)?.delete_stats_for_table(table_id)?;
    CatalogWriter::new(storage, txn, conn_txn)?.delete_table(table_id)?;
    Ok(())
}
