/// Fused index-range patch: B-tree range scan → group by page → patch fields
/// directly on heap pages without decoding rows. Eliminates:
/// - Full row decode (5000× for typical range)
/// - WHERE recheck (redundant, index already filtered)
/// - Double heap page read (candidate collection + patch phase)
#[allow(clippy::too_many_arguments)]
fn fused_index_range_patch(
    index_def: &axiomdb_catalog::IndexDef,
    lo: Option<&[u8]>,
    hi: Option<&[u8]>,
    assignments: &[(usize, Expr)],
    col_types: &[axiomdb_types::DataType],
    storage: &dyn StorageEngine,
    txn: &TxnManager,
    conn_txn: &mut ConnectionTxn,
    snap: axiomdb_core::TransactionSnapshot,
    resolved: &axiomdb_catalog::ResolvedTable,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let _txn_id = conn_txn.txn_id;

    // 1. B-tree range scan → collect RecordIds only.
    let pairs = BTree::range_in(storage, index_def.root_page_id, lo, hi)?;
    let rids: Vec<RecordId> = pairs.into_iter().map(|(rid, _)| rid).collect();

    if rids.is_empty() {
        return Ok(QueryResult::Affected {
            count: 0,
            last_insert_id: None,
        });
    }

    // 2. Group RIDs by page_id for sequential page access.
    let mut page_groups: std::collections::BTreeMap<u64, Vec<u16>> =
        std::collections::BTreeMap::new();
    for rid in &rids {
        page_groups
            .entry(rid.page_id)
            .or_default()
            .push(rid.slot_id);
    }

    // 3. For each page: read → patch visible matching slots → write.
    let hdr_size = std::mem::size_of::<axiomdb_storage::RowHeader>();
    let bitmap_len = col_types.len().div_ceil(8);
    let n_cols = col_types.len();

    #[allow(clippy::type_complexity)]
    let mut wal_images: Vec<(Vec<u8>, Vec<u8>, Vec<u8>, u64, u16)> = Vec::new();
    let mut matched = 0u64;
    let mut patched = 0u64;

    // Reusable sparse row for eval(): only populated at assigned column positions.
    let mut sparse_row: Vec<Value> = vec![Value::Null; n_cols];

    for (page_id, slot_ids) in &page_groups {
        let mut page = storage.read_page(*page_id)?.into_page();
        let mut page_dirty = false;

        for &slot_id in slot_ids {
            let entry = axiomdb_storage::heap::read_slot(&page, slot_id);
            if entry.is_dead() {
                continue;
            }
            let off = entry.offset as usize;
            let len = entry.length as usize;

            // MVCC visibility check (O(1): just txn_id comparisons).
            // Copy header fields to avoid holding an immutable borrow across mutations.
            let (hdr_visible, hdr_txn_created, hdr_version, hdr_flags) = {
                let hdr: &axiomdb_storage::RowHeader =
                    bytemuck::from_bytes(&page.as_bytes()[off..off + hdr_size]);
                (
                    hdr.is_visible(&snap),
                    hdr.txn_id_created,
                    hdr.row_version,
                    hdr._flags,
                )
            };
            if !hdr_visible {
                continue;
            }
            matched += 1;

            // Capture old tuple image BEFORE patching.
            let old_image = page.as_bytes()[off..off + len].to_vec();

            let row_start = off + hdr_size;
            let row_len = len - hdr_size;
            let bitmap = page.as_bytes()[row_start..row_start + bitmap_len].to_vec();

            // For each assignment: read field → eval → write field.
            let mut changed = false;
            for &(col_pos, ref val_expr) in assignments {
                let row_data = &page.as_bytes()[row_start..row_start + row_len];
                if let Some(loc) =
                    axiomdb_types::field_patch::compute_field_location_runtime(
                        col_types,
                        col_pos,
                        &bitmap,
                        Some(row_data),
                    )
                {
                    let current_val =
                        axiomdb_types::field_patch::read_field(row_data, &loc)?;
                    // Populate sparse row for eval, then reset after.
                    sparse_row[col_pos] = current_val.clone();
                    let new_val = eval(val_expr, &sparse_row)?;
                    sparse_row[col_pos] = Value::Null;

                    if new_val != current_val {
                        changed = true;
                        let row_mut =
                            &mut page.as_bytes_mut()[row_start..row_start + row_len];
                        let _ = axiomdb_types::field_patch::write_field(
                            row_mut, &loc, &new_val,
                        );
                    }
                }
            }

            if changed {
                // Increment row version, preserve txn_id_created.
                let new_hdr = axiomdb_storage::RowHeader {
                    txn_id_created: hdr_txn_created,
                    txn_id_deleted: 0,
                    row_version: hdr_version.wrapping_add(1),
                    _flags: hdr_flags,
                };
                page.as_bytes_mut()[off..off + hdr_size]
                    .copy_from_slice(bytemuck::bytes_of(&new_hdr));

                let new_image = page.as_bytes()[off..off + len].to_vec();
                wal_images.push((vec![], old_image, new_image, *page_id, slot_id));
                page.clear_all_visible();
                page_dirty = true;
                patched += 1;
            }
        }

        if page_dirty {
            page.update_checksum();
            storage.write_page(*page_id, &page)?;
        }
    }

    // WAL: batch UpdateInPlace with old+new tuple images.
    if !wal_images.is_empty() {
        type WalImageRef<'a> = (&'a [u8], &'a [u8], &'a [u8], u64, u16);
        let batch_refs: Vec<WalImageRef<'_>> = wal_images
            .iter()
            .map(|(k, old, new, pid, sid)| {
                (k.as_slice(), old.as_slice(), new.as_slice(), *pid, *sid)
            })
            .collect();
        let _ = txn.record_update_in_place_batch(conn_txn, resolved.def.id, &batch_refs);
    }

    if patched > 0 {
        ctx.stats.on_rows_changed(resolved.def.id, patched);
    }
    ctx.invalidate_all();

    Ok(QueryResult::Affected {
        count: matched,
        last_insert_id: None,
    })
}

#[derive(Debug, Clone)]
struct ClusteredUpdateCandidate {
    pk_key: Vec<u8>,
    row_header: axiomdb_storage::heap::RowHeader,
    row_data: Vec<u8>,
    values: Vec<Value>,
}

fn normalize_clustered_update_access_method(
    access_method: crate::planner::AccessMethod,
) -> crate::planner::AccessMethod {
    match access_method {
        crate::planner::AccessMethod::IndexOnlyScan {
            index_def, lo, hi, ..
        } => {
            let is_single_key_point = index_def.columns.len() == 1
                && hi
                    .as_ref()
                    .map(|bound| bound.as_slice() == lo.as_slice())
                    .unwrap_or(false);

            if is_single_key_point {
                crate::planner::AccessMethod::IndexLookup { index_def, key: lo }
            } else {
                crate::planner::AccessMethod::IndexRange {
                    index_def,
                    lo: Some(lo),
                    hi,
                }
            }
        }
        other => other,
    }
}

