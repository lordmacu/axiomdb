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
        storage: &dyn StorageEngine,
        table_def: &TableDef,
        columns: &[ColumnDef],
        snap: TransactionSnapshot,
        column_mask: Option<&[bool]>,
    ) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
        if table_def.is_clustered() {
            scan_clustered_table_masked(storage, table_def, columns, snap, column_mask)
        } else {
            Self::scan_table_direct(storage, table_def, columns, snap, column_mask)
        }
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
        storage: &dyn StorageEngine,
        table_def: &TableDef,
        columns: &[ColumnDef],
        snap: TransactionSnapshot,
        column_mask: Option<&[bool]>,
    ) -> Result<Vec<(RecordId, Vec<Value>)>, DbError> {
        ensure_heap_table(table_def, "SELECT from clustered table — Phase 39.15")?;
        let col_types = column_data_types(columns);
        let masked_decode = column_mask.filter(|mask| !mask.iter().all(|&b| b));
        let mut result = Vec::new();
        let mut current = table_def.root_page_id;

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
                let mut values = if let Some(mask) = masked_decode {
                    decode_row_masked(row_data, &col_types, mask)?
                } else {
                    decode_row(row_data, &col_types)?
                };
                // Phase 11.2: resolve TOAST placeholders from overflow chains.
                detoast_row(&mut values, storage);
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

    /// Scan with inline WHERE filter (Phase 8.1 — vectorized filter).
    ///
    /// Like `scan_table_direct` but evaluates the WHERE predicate INSIDE the
    /// page loop, skipping full row decode for non-matching rows. This is the
    /// "two-phase decode" approach inspired by DuckDB's vectorized filter:
    ///
    /// 1. For each visible slot: decode ALL columns (needed for WHERE + SELECT)
    /// 2. Evaluate WHERE predicate immediately
    /// 3. Only push passing rows to the result
    ///
    /// Why this helps: the `result.push()` + downstream `combined_rows.push()`
    /// are skipped for filtered-out rows, eliminating ~50% of Vec operations
    /// at 50% selectivity. The decode cost is the same, but allocation pressure
    /// is halved.
    ///
    /// Future: Phase 8.1b will decode WHERE columns separately from SELECT
    /// columns (true two-phase decode).
    /// Scan with inline WHERE filter + two-phase decode + selection mask.
    ///
    /// **Phase 1 (selection mask per page):** iterate all slots, collect
    /// visible slot offsets into a Vec without decoding any row data.
    ///
    /// **Phase 2 (two-phase decode):** for each visible slot:
    ///   a) If `where_col_mask` provided: decode only WHERE columns
    ///      (`decode_row_masked`), evaluate predicate. If fails → skip.
    ///   b) For passing rows: full `decode_row` to get all columns.
    ///
    /// This avoids full row decode + String allocations for filtered-out rows.
    /// Research: DuckDB SelectionVector + adaptive filter; PostgreSQL
    /// attcacheoff for selective column access; SQLite OP_Column lazy decode.
    pub fn scan_table_filtered<F>(
        storage: &dyn StorageEngine,
        table_def: &TableDef,
        columns: &[ColumnDef],
        snap: TransactionSnapshot,
        mut predicate: F,
        zone_map_pred: Option<(usize, &axiomdb_storage::zone_map::ZoneMapPredicate)>,
        where_col_mask: Option<&[bool]>,
        batch_pred: Option<&crate::eval::batch::BatchPredicate>,
    ) -> Result<Vec<(RecordId, Vec<Value>)>, DbError>
    where
        F: FnMut(&[Value]) -> bool,
    {
        Self::scan_table_filtered_brin(
            storage, table_def, columns, snap, &mut predicate,
            zone_map_pred, where_col_mask, batch_pred, None, 0,
        )
    }

    /// Like `scan_table_filtered` but with optional BRIN range skip set.
    ///
    /// When `brin_ranges` is `Some`, only pages whose range_id
    /// (`page_id / brin_ppr`) is in the set are scanned. Pages in
    /// non-qualifying ranges are skipped entirely (Phase 11.1b).
    #[expect(clippy::too_many_arguments, reason = "BRIN adds 2 params to existing 8-param scan")]
    pub fn scan_table_filtered_brin<F>(
        storage: &dyn StorageEngine,
        table_def: &TableDef,
        columns: &[ColumnDef],
        snap: TransactionSnapshot,
        predicate: &mut F,
        zone_map_pred: Option<(usize, &axiomdb_storage::zone_map::ZoneMapPredicate)>,
        where_col_mask: Option<&[bool]>,
        batch_pred: Option<&crate::eval::batch::BatchPredicate>,
        brin_ranges: Option<&std::collections::HashSet<u32>>,
        brin_ppr: u32,
    ) -> Result<Vec<(RecordId, Vec<Value>)>, DbError>
    where
        F: FnMut(&[Value]) -> bool,
    {
        ensure_heap_table(table_def, "SELECT from clustered table — Phase 39.15")?;
        let col_types = column_data_types(columns);
        let has_two_phase = where_col_mask
            .filter(|m| !m.iter().all(|&b| b))
            .is_some();
        let mut result = Vec::new();
        let mut current = table_def.root_page_id;

        while current != 0 {
            let raw = *storage.read_page(current)?.as_bytes();
            let page = Page::from_bytes(raw)?;
            let next = heap_chain::chain_next_page(&page);

            if next != 0 {
                storage.prefetch_hint(next, 1);
            }

            // BRIN range skip (Phase 11.1b).
            if let Some(ranges) = brin_ranges {
                if brin_ppr > 0 {
                    let range_id = (current / brin_ppr as u64) as u32;
                    if !ranges.contains(&range_id) {
                        current = next;
                        continue;
                    }
                }
            }

            // Zone map skip (Phase 8.3b).
            // Only skip if the zone map's tracked column matches the predicate column.
            if let Some((pred_col_idx, zmp)) = zone_map_pred {
                if let Some(zm) = axiomdb_storage::zone_map::read_zone_map(&page) {
                    if zm.col_idx as usize == pred_col_idx
                        && !axiomdb_storage::zone_map::zone_map_might_match(&zm, zmp)
                    {
                        current = next;
                        continue;
                    }
                }
            }

            // ── Phase 1: Selection mask — collect visible slot offsets ────
            // Single pass over slot array: only RowHeader check, no decode.
            let num = num_slots(&page);
            let mut visible_slots: Vec<(u16, usize, usize)> = Vec::new(); // (slot_id, off, len)
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
                visible_slots.push((slot_id, off, len));
            }

            // ── Phase 2: Predicate evaluation + decode for visible slots ──
            // ── Phase 2: Predicate evaluation + decode ────────────────────
            //
            // Phase 8.2 SIMD batch path: gather column values from all visible
            // rows into contiguous arrays, SIMD-compare (8×i32 on AVX2, 4×i32
            // on NEON), decode only passing rows. Falls back to per-row scalar
            // when batch_pred is None.
            if let Some(bp) = batch_pred {
                let page_bytes = page.as_bytes();
                let hdr = size_of::<RowHeader>();
                let row_slices: Vec<&[u8]> = visible_slots
                    .iter()
                    .map(|&(_, off, len)| &page_bytes[off + hdr..off + len])
                    .collect();
                let mut passed = vec![true; row_slices.len()];
                bp.eval_batch(&row_slices, &mut passed);

                for (i, &(slot_id, off, len)) in visible_slots.iter().enumerate() {
                    if passed[i] {
                        let row_data = &page_bytes[off + hdr..off + len];
                        let mut values = decode_row(row_data, &col_types)?;
                        detoast_row(&mut values, storage);
                        result.push((
                            RecordId {
                                page_id: current,
                                slot_id,
                            },
                            values,
                        ));
                    }
                }
            } else {
                // Scalar fallback: per-row decode + predicate evaluation.
                for &(slot_id, off, len) in &visible_slots {
                    let row_data = &page.as_bytes()[off + size_of::<RowHeader>()..off + len];

                    if has_two_phase {
                        let partial =
                            decode_row_masked(row_data, &col_types, where_col_mask.unwrap())?;
                        if !predicate(&partial) {
                            continue;
                        }
                        let mut values = decode_row(row_data, &col_types)?;
                        detoast_row(&mut values, storage);
                        result.push((
                            RecordId {
                                page_id: current,
                                slot_id,
                            },
                            values,
                        ));
                    } else {
                        let mut values = decode_row(row_data, &col_types)?;
                        detoast_row(&mut values, storage);
                        if !predicate(&values) {
                            continue;
                        }
                        result.push((
                            RecordId {
                                page_id: current,
                                slot_id,
                            },
                            values,
                        ));
                    }
                }
            }

            current = next;
        }

        Ok(result)
    }

    // ── Parallel scan (Phase 9.1) ──────────────────────────────────────────

    /// Minimum pages before engaging Rayon parallelism. Below this threshold,
    /// thread spawn overhead exceeds the per-page decode savings.
    const PARALLEL_MIN_PAGES: usize = 4;

    /// Parallel filtered scan — distributes per-page decode+filter across
    /// Rayon's thread pool (morsel-driven, DuckDB-inspired).
    ///
    /// **Phase 1** (serial): walk heap chain to collect page IDs.
    /// **Phase 2** (parallel): `par_iter()` over pages — each thread reads,
    /// applies zone map skip + BatchPredicate, decodes passing rows.
    /// **Phase 3** (serial): flatten per-thread results.
    ///
    /// Falls back to single-threaded `scan_table_filtered` when the table
    /// has fewer than `PARALLEL_MIN_PAGES` pages.
    ///
    /// Results may be in different order than single-threaded scan — callers
    /// needing ORDER BY must sort after this call.
    /// Phase 9.11: `scan_limit` enables early-exit scanning (PostgreSQL's
    /// `ExecutorRun(count)` pattern). When `Some(n)`, the scan stops after
    /// collecting n passing rows — avoids scanning the full table for
    /// `SELECT ... LIMIT n` without ORDER BY. `None` means scan all rows.
    #[allow(clippy::too_many_arguments)]
    pub fn scan_table_filtered_parallel<F>(
        storage: &dyn StorageEngine,
        table_def: &TableDef,
        columns: &[ColumnDef],
        snap: TransactionSnapshot,
        predicate: F,
        zone_map_pred: Option<(usize, &axiomdb_storage::zone_map::ZoneMapPredicate)>,
        batch_pred: Option<&crate::eval::batch::BatchPredicate>,
        decode_mask: Option<&[bool]>,
        scan_limit: Option<usize>,
    ) -> Result<Vec<(RecordId, Vec<Value>)>, DbError>
    where
        F: Fn(&[Value]) -> bool + Send + Sync,
    {
        ensure_heap_table(table_def, "SELECT from clustered table — Phase 39.15")?;
        use rayon::prelude::*;

        let col_types = column_data_types(columns);

        // Phase 1: serial — collect all page IDs by walking the heap chain.
        let page_ids = Self::collect_page_ids(storage, table_def.root_page_id)?;

        if page_ids.len() < Self::PARALLEL_MIN_PAGES {
            // Small table: serial path (avoid Rayon overhead).
            return Self::scan_table_filtered(
                storage,
                table_def,
                columns,
                snap,
                predicate,
                zone_map_pred,
                None, // where_col_mask not needed with batch_pred
                batch_pred,
            );
        }

        // Phase 2: parallel — process each page independently.
        #[allow(clippy::type_complexity)]
        let results: Result<Vec<Vec<(RecordId, Vec<Value>)>>, DbError> = page_ids
            .par_iter()
            .map(|&page_id| {
                Self::process_page_filtered(
                    storage,
                    page_id,
                    snap.clone(),
                    &col_types,
                    &predicate,
                    zone_map_pred,
                    batch_pred,
                    decode_mask,
                )
            })
            .collect();

        // Phase 3: flatten per-thread results + apply scan limit.
        // PostgreSQL's ExecutePlan uses `numberTuples` to stop after limit rows.
        let flat: Vec<_> = results?.into_iter().flatten().collect();
        match scan_limit {
            Some(limit) if flat.len() > limit => Ok(flat.into_iter().take(limit).collect()),
            _ => Ok(flat),
        }
    }

    /// Collects all heap chain page IDs starting from `root`.
    fn collect_page_ids(storage: &dyn StorageEngine, root: u64) -> Result<Vec<u64>, DbError> {
        let mut ids = Vec::new();
        let mut current = root;
        while current != 0 {
            ids.push(current);
            let raw = *storage.read_page(current)?.as_bytes();
            let page = Page::from_bytes(raw)?;
            current = heap_chain::chain_next_page(&page);
        }
        Ok(ids)
    }

    /// Processes a single heap page: visibility check → zone map → batch
    /// predicate → decode passing rows. Called from parallel scan.
    fn process_page_filtered<F>(
        storage: &dyn StorageEngine,
        page_id: u64,
        snap: TransactionSnapshot,
        col_types: &[DataType],
        predicate: &F,
        zone_map_pred: Option<(usize, &axiomdb_storage::zone_map::ZoneMapPredicate)>,
        batch_pred: Option<&crate::eval::batch::BatchPredicate>,
        decode_mask: Option<&[bool]>,
    ) -> Result<Vec<(RecordId, Vec<Value>)>, DbError>
    where
        F: Fn(&[Value]) -> bool,
    {
        let raw = *storage.read_page(page_id)?.as_bytes();
        let page = Page::from_bytes(raw)?;

        // Zone map skip.
        if let Some((pred_col_idx, zmp)) = zone_map_pred {
            if let Some(zm) = axiomdb_storage::zone_map::read_zone_map(&page) {
                if zm.col_idx as usize == pred_col_idx
                    && !axiomdb_storage::zone_map::zone_map_might_match(&zm, zmp)
                {
                    return Ok(Vec::new());
                }
            }
        }

        // Selection mask: collect visible slots.
        let num = num_slots(&page);
        let mut visible_slots: Vec<(u16, usize, usize)> = Vec::new();
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
            visible_slots.push((slot_id, off, len));
        }

        if visible_slots.is_empty() {
            return Ok(Vec::new());
        }

        let hdr = size_of::<RowHeader>();
        let page_bytes = page.as_bytes();

        // BatchPredicate SIMD batch path.
        if let Some(bp) = batch_pred {
            let row_slices: Vec<&[u8]> = visible_slots
                .iter()
                .map(|&(_, off, len)| &page_bytes[off + hdr..off + len])
                .collect();
            let mut passed = vec![true; row_slices.len()];
            bp.eval_batch(&row_slices, &mut passed);

            let mut result = Vec::new();
            for (i, &(slot_id, off, len)) in visible_slots.iter().enumerate() {
                if passed[i] {
                    let row_data = &page_bytes[off + hdr..off + len];
                    // Phase 9.2: decode only columns in the unified mask
                    // (SELECT ∪ WHERE ∪ ORDER BY ∪ GROUP BY). Non-masked
                    // columns get Value::Null — saves String/Text allocation.
                    let values = if let Some(mask) = decode_mask {
                        decode_row_masked(row_data, col_types, mask)?
                    } else {
                        decode_row(row_data, col_types)?
                    };
                    result.push((RecordId { page_id, slot_id }, values));
                }
            }
            return Ok(result);
        }

        // Scalar fallback: per-row decode + predicate.
        let mut result = Vec::new();
        for &(slot_id, off, len) in &visible_slots {
            let row_data = &page_bytes[off + hdr..off + len];
            let values = decode_row(row_data, col_types)?;
            if predicate(&values) {
                result.push((RecordId { page_id, slot_id }, values));
            }
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

}
