// ── Remove ───────────────────────────────────────────────────────────────────

/// Remove the cell at logical index `pos`. Adds the freed space to the
/// freeblock chain for reuse.
pub fn remove_cell(page: &mut Page, pos: usize) -> Result<(), DbError> {
    let n = num_cells(page) as usize;
    if pos >= n {
        return Err(DbError::Other(format!(
            "clustered_leaf: remove pos {pos} >= num_cells {n}"
        )));
    }

    let body_off = cell_ptr_at(page, pos as u16);
    let csize = cell_size_at(page, body_off);

    // Add freed space to freeblock chain (if large enough).
    add_freeblock(page, body_off, csize);
    // Fragments < MIN_FREEBLOCK are lost until defragmentation.

    // Shift cell pointers left by 2 bytes to close the gap.
    let ptr_base = HEADER_SIZE + CELL_PTR_START;
    let dst = ptr_base + pos * CELL_PTR_SIZE;
    let src = dst + CELL_PTR_SIZE;
    let count = (n - 1 - pos) * CELL_PTR_SIZE;
    if count > 0 {
        page.as_bytes_mut().copy_within(src..src + count, dst);
    }

    set_num_cells(page, (n - 1) as u16);
    Ok(())
}

// ── Direct header patch (InnoDB-inspired) ────────────────────────────────────

/// Patches `txn_id_deleted` in the RowHeader of cell at logical index `idx`
/// directly on the page buffer. Only modifies 8 bytes — no cell copy, no
/// rewrite, no allocation. InnoDB equivalent: `mtr->write<1>()` for delete flag.
///
/// Also returns the old txn_id_deleted value for WAL undo.
pub fn patch_txn_id_deleted(
    page: &mut Page,
    idx: usize,
    new_txn_id_deleted: u64,
) -> Result<u64, DbError> {
    let n = num_cells(page) as usize;
    if idx >= n {
        return Err(DbError::Other(format!(
            "clustered_leaf: patch idx {idx} >= num_cells {n}"
        )));
    }
    let body_off = cell_ptr_at(page, idx as u16) as usize;
    // txn_id_deleted is at: cell_start + CELL_META_SIZE(6) + 8 (after txn_id_created)
    let txn_deleted_off = HEADER_SIZE + body_off + CELL_META_SIZE + 8;

    let b = page.as_bytes();
    let old_val = u64::from_le_bytes([
        b[txn_deleted_off],
        b[txn_deleted_off + 1],
        b[txn_deleted_off + 2],
        b[txn_deleted_off + 3],
        b[txn_deleted_off + 4],
        b[txn_deleted_off + 5],
        b[txn_deleted_off + 6],
        b[txn_deleted_off + 7],
    ]);

    page.as_bytes_mut()[txn_deleted_off..txn_deleted_off + 8]
        .copy_from_slice(&new_txn_id_deleted.to_le_bytes());

    Ok(old_val)
}

/// Returns the absolute byte offset within `page.as_bytes()` where the
/// `row_data` region begins for the cell at logical index `cell_idx`, plus
/// the `key_len` stored in the cell header.
///
/// Formula:
/// ```text
/// row_data_abs_off = HEADER_SIZE + body_off + CELL_META_SIZE + ROW_HEADER_SIZE + key_len
///                  = HEADER_SIZE + body_off + 6 + 24 + key_len
/// ```
///
/// Used by `patch_field_in_place` and `update_row_header_in_place` to locate
/// the exact page-buffer position of a field without cloning row data.
pub fn cell_row_data_abs_off(page: &Page, cell_idx: usize) -> Result<(usize, usize), DbError> {
    let n = num_cells(page) as usize;
    if cell_idx >= n {
        return Err(DbError::Other(format!(
            "clustered_leaf: cell_row_data_abs_off idx {cell_idx} >= num_cells {n}"
        )));
    }
    let body_off = cell_ptr_at(page, cell_idx as u16) as usize;
    let abs = HEADER_SIZE + body_off;
    if abs + CELL_META_SIZE > PAGE_SIZE {
        return Err(DbError::Other(
            "clustered_leaf: cell header truncated in cell_row_data_abs_off".into(),
        ));
    }
    let b = page.as_bytes();
    let key_len = u16::from_le_bytes([b[abs], b[abs + 1]]) as usize;
    let row_data_abs = abs + CELL_META_SIZE + ROW_HEADER_SIZE + key_len;
    if row_data_abs > PAGE_SIZE {
        return Err(DbError::Other(
            "clustered_leaf: row_data_abs_off exceeds PAGE_SIZE".into(),
        ));
    }
    Ok((row_data_abs, key_len))
}

/// Writes `new_bytes` directly at absolute page offset `field_abs_off`.
///
/// This is the AxiomDB equivalent of InnoDB's
/// `mtr->memcpy(block, rec + off, buf, len)` from `btr_cur_upd_rec_in_place`:
/// only the changed bytes are touched — no cell re-encode, no allocation.
///
/// # Preconditions (caller's responsibility)
///
/// - `field_abs_off` must lie within the `row_data` region of an **inline**
///   (non-overflow) cell. Overflow cells keep part of their data in the
///   overflow chain; patching them through this function would corrupt the row.
/// - `new_bytes.len()` must equal the fixed encoded size of the column type
///   being updated (1 for Bool, 4 for Int/Date, 8 for BigInt/Real/Timestamp).
/// - The caller must call `page.update_checksum()` before writing the page to
///   storage (typically once per leaf after all patches on that leaf).
pub fn patch_field_in_place(
    page: &mut Page,
    field_abs_off: usize,
    new_bytes: &[u8],
) -> Result<(), DbError> {
    let end = field_abs_off
        .checked_add(new_bytes.len())
        .ok_or_else(|| DbError::Other("patch_field_in_place: offset overflow".into()))?;
    if end > PAGE_SIZE {
        return Err(DbError::Other(format!(
            "patch_field_in_place: field [{field_abs_off}..{end}) exceeds PAGE_SIZE {PAGE_SIZE}"
        )));
    }
    page.as_bytes_mut()[field_abs_off..end].copy_from_slice(new_bytes);
    Ok(())
}

/// Writes a new `RowHeader` at the header slot of the cell at logical index
/// `cell_idx`.
///
/// Serializes to a `[u8; ROW_HEADER_SIZE]` stack buffer before writing because
/// cells are not guaranteed to be 8-byte aligned in the page body — a direct
/// `bytemuck::bytes_of` on a misaligned destination pointer would be UB.
/// This mirrors the read path in `read_cell_at_offset`, which copies to a
/// stack buffer before `bytemuck::from_bytes` for the same reason.
///
/// Only the four RowHeader fields are touched:
/// `txn_id_created`, `txn_id_deleted`, `row_version`, `_flags`.
/// Key and row_data bytes are left completely unchanged.
pub fn update_row_header_in_place(
    page: &mut Page,
    cell_idx: usize,
    new_header: &RowHeader,
) -> Result<(), DbError> {
    let n = num_cells(page) as usize;
    if cell_idx >= n {
        return Err(DbError::Other(format!(
            "update_row_header_in_place: cell_idx {cell_idx} >= num_cells {n}"
        )));
    }
    let body_off = cell_ptr_at(page, cell_idx as u16) as usize;
    let hdr_abs = HEADER_SIZE + body_off + CELL_META_SIZE;
    let hdr_end = hdr_abs + ROW_HEADER_SIZE;
    if hdr_end > PAGE_SIZE {
        return Err(DbError::Other(
            "update_row_header_in_place: header slot exceeds PAGE_SIZE".into(),
        ));
    }
    // Serialize to an aligned stack buffer before writing to the (potentially
    // unaligned) page position. Always little-endian, matching the codec.
    let mut buf = [0u8; ROW_HEADER_SIZE];
    buf[0..8].copy_from_slice(&new_header.txn_id_created.to_le_bytes());
    buf[8..16].copy_from_slice(&new_header.txn_id_deleted.to_le_bytes());
    buf[16..20].copy_from_slice(&new_header.row_version.to_le_bytes());
    buf[20..24].copy_from_slice(&new_header._flags.to_le_bytes());
    page.as_bytes_mut()[hdr_abs..hdr_end].copy_from_slice(&buf);
    Ok(())
}

// ── Defragment ───────────────────────────────────────────────────────────────

/// Compact all live cells to the end of the page body, eliminating all
/// freeblocks and fragmentation. Cell pointer array order is preserved.
pub fn defragment(page: &mut Page) {
    let n = num_cells(page) as usize;
    if n == 0 {
        set_cell_content_start(page, BODY_SIZE as u16);
        set_freeblock_offset(page, 0);
        return;
    }

    // Collect all live cell data into a temporary buffer.
    // Each entry: (pointer_index, cell_bytes).
    let mut cell_data: Vec<(usize, Vec<u8>)> = Vec::with_capacity(n);
    for i in 0..n {
        let off = cell_ptr_at(page, i as u16);
        let size = cell_size_at(page, off);
        let abs = HEADER_SIZE + off as usize;
        cell_data.push((i, page.as_bytes()[abs..abs + size].to_vec()));
    }

    // Rewrite cells contiguously from the end of the body.
    // Process in reverse logical order so that cell 0 ends up closest to the
    // cell content start (lowest body offset).
    let mut write_pos = BODY_SIZE;
    let mut new_offsets = vec![0u16; n];
    for &(idx, ref data) in cell_data.iter().rev() {
        write_pos -= data.len();
        let dst_abs = HEADER_SIZE + write_pos;
        page.as_bytes_mut()[dst_abs..dst_abs + data.len()].copy_from_slice(data);
        new_offsets[idx] = write_pos as u16;
    }

    // Update cell pointers.
    for (i, &off) in new_offsets.iter().enumerate() {
        set_cell_ptr_at(page, i as u16, off);
    }

    set_cell_content_start(page, write_pos as u16);
    set_freeblock_offset(page, 0);
}

fn cell_image_at(page: &Page, body_off: u16) -> Result<Vec<u8>, DbError> {
    let size = cell_size_at(page, body_off);
    let abs = HEADER_SIZE + body_off as usize;
    if abs + size > PAGE_SIZE {
        return Err(DbError::Other(
            "clustered_leaf: cell image extends beyond page boundary".into(),
        ));
    }
    Ok(page.as_bytes()[abs..abs + size].to_vec())
}

fn write_cell_image(page: &mut Page, body_off: u16, image: &[u8]) {
    let abs = HEADER_SIZE + body_off as usize;
    page.as_bytes_mut()[abs..abs + image.len()].copy_from_slice(image);
}

fn encode_cell_image(
    key: &[u8],
    row_header: &RowHeader,
    total_row_len: usize,
    local_row_data: &[u8],
    overflow_first_page: Option<u64>,
) -> Result<Vec<u8>, DbError> {
    validate_cell_descriptor(key, total_row_len, local_row_data, overflow_first_page)?;

    let mut image = vec![
        0u8;
        CELL_META_SIZE
            + ROW_HEADER_SIZE
            + key.len()
            + local_row_data.len()
            + usize::from(overflow_first_page.is_some()) * OVERFLOW_PTR_SIZE
    ];
    image[..2].copy_from_slice(&(key.len() as u16).to_le_bytes());
    image[2..6].copy_from_slice(&(total_row_len as u32).to_le_bytes());
    image[CELL_META_SIZE..CELL_META_SIZE + ROW_HEADER_SIZE]
        .copy_from_slice(bytemuck::bytes_of(row_header));
    let key_start = CELL_META_SIZE + ROW_HEADER_SIZE;
    image[key_start..key_start + key.len()].copy_from_slice(key);
    let row_start = key_start + key.len();
    image[row_start..row_start + local_row_data.len()].copy_from_slice(local_row_data);
    if let Some(first_page) = overflow_first_page {
        let overflow_start = row_start + local_row_data.len();
        image[overflow_start..overflow_start + OVERFLOW_PTR_SIZE]
            .copy_from_slice(&first_page.to_le_bytes());
    }
    Ok(image)
}

fn collect_cells(page: &Page) -> Result<Vec<OwnedCell>, DbError> {
    let n = num_cells(page) as usize;
    let mut cells = Vec::with_capacity(n);
    for idx in 0..n {
        let cell = read_cell(page, idx as u16)?;
        cells.push(OwnedCell {
            key: cell.key.to_vec(),
            row_header: cell.row_header,
            total_row_len: cell.total_row_len,
            row_data: cell.row_data.to_vec(),
            overflow_first_page: cell.overflow_first_page,
        });
    }
    Ok(cells)
}

fn validate_cell_descriptor(
    key: &[u8],
    total_row_len: usize,
    local_row_data: &[u8],
    overflow_first_page: Option<u64>,
) -> Result<(), DbError> {
    if key.len() > u16::MAX as usize {
        return Err(DbError::KeyTooLong {
            len: key.len(),
            max: u16::MAX as usize,
        });
    }
    if total_row_len > u32::MAX as usize {
        return Err(DbError::ValueTooLarge {
            len: total_row_len,
            max: u32::MAX as usize,
        });
    }

    let expected_local_len = local_row_len(key.len(), total_row_len);
    if local_row_data.len() != expected_local_len {
        return Err(DbError::Other(format!(
            "clustered_leaf: descriptor local length mismatch for key_len={} total_row_len={total_row_len}: expected {expected_local_len}, got {}",
            key.len(),
            local_row_data.len()
        )));
    }

    let needs_overflow = total_row_len > expected_local_len;
    if needs_overflow != overflow_first_page.is_some() {
        return Err(DbError::Other(format!(
            "clustered_leaf: descriptor overflow mismatch for key_len={} total_row_len={total_row_len}",
            key.len()
        )));
    }

    if cell_footprint(key.len(), total_row_len) > page_capacity_bytes() {
        return Err(DbError::HeapPageFull {
            page_id: 0,
            needed: cell_footprint(key.len(), total_row_len),
            available: page_capacity_bytes(),
        });
    }

    Ok(())
}

