// ── Initialization ───────────────────────────────────────────────────────────

/// Initialize a page as an empty clustered leaf.
pub fn init_clustered_leaf(page: &mut Page) {
    // Set page type in the page header.
    page.header_mut().page_type = PageType::ClusteredLeaf as u8;

    // Write clustered leaf header in the body.
    let b = page.as_bytes_mut();

    // is_leaf = 1
    b[HEADER_SIZE] = 1;
    // _pad0
    b[HEADER_SIZE + 1] = 0;

    set_num_cells(page, 0);
    set_cell_content_start(page, BODY_SIZE as u16);
    set_freeblock_offset(page, 0);
    set_next_leaf(page, NULL_PAGE);
}

// ── Binary search ────────────────────────────────────────────────────────────

/// Binary search for `key` in the cell pointer array.
///
/// Returns `Ok(idx)` if an exact match is found, or `Err(insert_pos)` where
/// `insert_pos` is the index at which the key should be inserted.
pub fn search(page: &Page, key: &[u8]) -> Result<usize, usize> {
    let n = num_cells(page) as usize;
    if n == 0 {
        return Err(0);
    }
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let cell_off = cell_ptr_at(page, mid as u16);
        let cell_key = cell_key_at(page, cell_off);
        match cell_key.cmp(key) {
            std::cmp::Ordering::Equal => return Ok(mid),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    Err(lo)
}

// ── Space management ─────────────────────────────────────────────────────────

/// Total free space: gap between pointer array end and cell content start,
/// plus all freeblock bytes.
pub fn free_space(page: &Page) -> usize {
    let gap = gap_space(page);
    let fb = total_freeblock_space(page);
    gap + fb
}

fn add_freeblock(page: &mut Page, body_off: u16, size: usize) {
    if size < MIN_FREEBLOCK {
        return;
    }

    let old_head = freeblock_offset(page);
    let abs = HEADER_SIZE + body_off as usize;
    let b = page.as_bytes_mut();
    b[abs..abs + 2].copy_from_slice(&old_head.to_le_bytes());
    b[abs + 2..abs + 4].copy_from_slice(&(size as u16).to_le_bytes());
    if size > MIN_FREEBLOCK {
        b[abs + MIN_FREEBLOCK..abs + size].fill(0);
    }
    set_freeblock_offset(page, body_off);
}

/// Gap space only (contiguous, between pointer array and cell content).
/// This is the space available for a new cell + its 2B pointer without defrag.
fn gap_space(page: &Page) -> usize {
    let ptr_end = cell_ptr_array_end(page);
    let content_start = cell_content_start(page) as usize;
    content_start.saturating_sub(ptr_end)
}

/// Sum of all freeblock sizes.
/// Sum of all freeblock sizes on this page. Used by VACUUM to decide
/// whether defragmentation is worthwhile.
pub fn total_freeblock_space(page: &Page) -> usize {
    let mut total = 0usize;
    let mut fb_off = freeblock_offset(page);
    while fb_off != 0 {
        let abs = HEADER_SIZE + fb_off as usize;
        let b = page.as_bytes();
        if abs + MIN_FREEBLOCK > PAGE_SIZE {
            break;
        }
        let block_size = u16::from_le_bytes([b[abs + 2], b[abs + 3]]) as usize;
        total += block_size;
        fb_off = u16::from_le_bytes([b[abs], b[abs + 1]]);
    }
    total
}

/// Try to allocate `size` bytes from the freeblock chain.
/// Returns the body-relative offset of the allocated block, or None.
fn allocate_from_freeblocks(page: &mut Page, size: usize) -> Option<u16> {
    let mut prev_off: Option<u16> = None; // body-relative offset of previous fb's next field
    let mut fb_off = freeblock_offset(page);

    while fb_off != 0 {
        let abs = HEADER_SIZE + fb_off as usize;
        let b = page.as_bytes();
        if abs + MIN_FREEBLOCK > PAGE_SIZE {
            break;
        }
        let next = u16::from_le_bytes([b[abs], b[abs + 1]]);
        let block_size = u16::from_le_bytes([b[abs + 2], b[abs + 3]]) as usize;

        if block_size >= size {
            let remainder = block_size - size;
            if remainder >= MIN_FREEBLOCK {
                // Split: keep remainder as a smaller freeblock at fb_off + size.
                let new_fb_off = fb_off + size as u16;
                let b = page.as_bytes_mut();
                let new_abs = HEADER_SIZE + new_fb_off as usize;
                b[new_abs..new_abs + 2].copy_from_slice(&next.to_le_bytes());
                b[new_abs + 2..new_abs + 4].copy_from_slice(&(remainder as u16).to_le_bytes());
                // Update previous pointer to new freeblock.
                if let Some(prev) = prev_off {
                    let prev_abs = HEADER_SIZE + prev as usize;
                    b[prev_abs..prev_abs + 2].copy_from_slice(&new_fb_off.to_le_bytes());
                } else {
                    set_freeblock_offset(page, new_fb_off);
                }
            } else {
                // Use entire block.
                if let Some(prev) = prev_off {
                    let b = page.as_bytes_mut();
                    let prev_abs = HEADER_SIZE + prev as usize;
                    b[prev_abs..prev_abs + 2].copy_from_slice(&next.to_le_bytes());
                } else {
                    set_freeblock_offset(page, next);
                }
            }
            return Some(fb_off);
        }

        prev_off = Some(fb_off);
        fb_off = next;
    }
    None
}

// ── Insert ───────────────────────────────────────────────────────────────────

/// Insert a cell at sorted position `pos` (0 = before all, num_cells = after all).
///
/// Returns `Err(DbError::HeapPageFull)` if the cell doesn't fit even after
/// checking freeblocks. The caller should defragment or split.
pub fn insert_cell(
    page: &mut Page,
    pos: usize,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<(), DbError> {
    if requires_overflow(key.len(), row_data.len()) {
        return Err(DbError::ValueTooLarge {
            len: row_data.len(),
            max: max_inline_row_bytes(key.len()).unwrap_or(0),
        });
    }

    insert_cell_with_overflow(page, pos, key, row_header, row_data.len(), row_data, None)
}

/// Insert a clustered leaf cell from a pre-materialized physical descriptor.
pub fn insert_cell_with_overflow(
    page: &mut Page,
    pos: usize,
    key: &[u8],
    row_header: &RowHeader,
    total_row_len: usize,
    local_row_data: &[u8],
    overflow_first_page: Option<u64>,
) -> Result<(), DbError> {
    validate_cell_descriptor(key, total_row_len, local_row_data, overflow_first_page)?;

    let cell_size = CELL_META_SIZE
        + ROW_HEADER_SIZE
        + key.len()
        + local_row_data.len()
        + usize::from(overflow_first_page.is_some()) * OVERFLOW_PTR_SIZE;
    let need_gap = CELL_PTR_SIZE; // 2 bytes for the new pointer
    let n = num_cells(page) as usize;

    if pos > n {
        return Err(DbError::Other(format!(
            "clustered_leaf: insert pos {pos} > num_cells {n}"
        )));
    }

    // Try to allocate cell space from freeblock chain first.
    let cell_offset = if let Some(fb_off) = allocate_from_freeblocks(page, cell_size) {
        // Got space from freeblock — still need gap space for the pointer.
        if gap_space(page) < need_gap {
            // Not enough room for pointer even though cell fits. Rare edge case.
            return Err(DbError::HeapPageFull {
                page_id: page.header().page_id,
                needed: need_gap,
                available: gap_space(page),
            });
        }
        fb_off
    } else {
        // Allocate from gap (contiguous free space).
        let total_need = cell_size + need_gap;
        let gap = gap_space(page);
        if gap < total_need {
            return Err(DbError::HeapPageFull {
                page_id: page.header().page_id,
                needed: total_need,
                available: gap,
            });
        }
        // Grow cell content area leftward.
        let new_start = cell_content_start(page) as usize - cell_size;
        set_cell_content_start(page, new_start as u16);
        new_start as u16
    };

    // Write cell data at the allocated body-relative offset.
    let abs = HEADER_SIZE + cell_offset as usize;
    let b = page.as_bytes_mut();
    b[abs..abs + 2].copy_from_slice(&(key.len() as u16).to_le_bytes());
    b[abs + 2..abs + 6].copy_from_slice(&(total_row_len as u32).to_le_bytes());
    b[abs + CELL_META_SIZE..abs + CELL_META_SIZE + ROW_HEADER_SIZE]
        .copy_from_slice(bytemuck::bytes_of(row_header));
    let key_start = abs + CELL_META_SIZE + ROW_HEADER_SIZE;
    b[key_start..key_start + key.len()].copy_from_slice(key);
    let row_start = key_start + key.len();
    b[row_start..row_start + local_row_data.len()].copy_from_slice(local_row_data);
    if let Some(first_page) = overflow_first_page {
        let overflow_start = row_start + local_row_data.len();
        b[overflow_start..overflow_start + OVERFLOW_PTR_SIZE]
            .copy_from_slice(&first_page.to_le_bytes());
    }

    // Shift cell pointers right by 2 bytes to make room at `pos`.
    let ptr_base = HEADER_SIZE + CELL_PTR_START;
    let src = ptr_base + pos * CELL_PTR_SIZE;
    let dst = src + CELL_PTR_SIZE;
    let count = (n - pos) * CELL_PTR_SIZE;
    if count > 0 {
        page.as_bytes_mut().copy_within(src..src + count, dst);
    }

    // Write new cell pointer at `pos`.
    set_cell_ptr_at(page, pos as u16, cell_offset);
    set_num_cells(page, (n + 1) as u16);

    Ok(())
}

/// Rewrites the cell at logical index `pos` while preserving its key and slot.
///
/// Returns the previous encoded cell image on success. If the replacement row
/// does not fit in the same leaf page even after rebuilding the leaf contents
/// compactly, returns `Ok(None)` and leaves the page unchanged.
pub fn rewrite_cell_same_key(
    page: &mut Page,
    pos: usize,
    expected_key: &[u8],
    new_row_header: &RowHeader,
    new_row_data: &[u8],
) -> Result<Option<Vec<u8>>, DbError> {
    if requires_overflow(expected_key.len(), new_row_data.len()) {
        return Err(DbError::ValueTooLarge {
            len: new_row_data.len(),
            max: max_inline_row_bytes(expected_key.len()).unwrap_or(0),
        });
    }

    rewrite_cell_same_key_with_overflow(
        page,
        pos,
        expected_key,
        new_row_header,
        new_row_data.len(),
        new_row_data,
        None,
    )
}

/// Rewrites the cell at logical index `pos` from a pre-materialized physical
/// descriptor while preserving its key and slot.
pub fn rewrite_cell_same_key_with_overflow(
    page: &mut Page,
    pos: usize,
    expected_key: &[u8],
    new_row_header: &RowHeader,
    total_row_len: usize,
    local_row_data: &[u8],
    overflow_first_page: Option<u64>,
) -> Result<Option<Vec<u8>>, DbError> {
    validate_cell_descriptor(
        expected_key,
        total_row_len,
        local_row_data,
        overflow_first_page,
    )?;

    let n = num_cells(page) as usize;
    if pos >= n {
        return Err(DbError::Other(format!(
            "clustered_leaf: rewrite pos {pos} >= num_cells {n}"
        )));
    }

    let body_off = cell_ptr_at(page, pos as u16);
    let old_size = cell_size_at(page, body_off);
    let old_cell = read_cell(page, pos as u16)?;
    if old_cell.key != expected_key {
        return Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered_leaf rewrite key mismatch at pos {pos}: expected {:?}, found {:?}",
                expected_key, old_cell.key
            ),
        });
    }

    let old_image = cell_image_at(page, body_off)?;
    let new_image = encode_cell_image(
        expected_key,
        new_row_header,
        total_row_len,
        local_row_data,
        overflow_first_page,
    )?;
    let new_size = new_image.len();

    if new_size <= old_size {
        write_cell_image(page, body_off, &new_image);
        if new_size < old_size {
            let free_off = body_off + new_size as u16;
            page.as_bytes_mut()
                [HEADER_SIZE + free_off as usize..HEADER_SIZE + body_off as usize + old_size]
                .fill(0);
            add_freeblock(page, free_off, old_size - new_size);
        }
        return Ok(Some(old_image));
    }

    let mut cells = collect_cells(page)?;
    cells[pos] = OwnedCell {
        key: expected_key.to_vec(),
        row_header: *new_row_header,
        total_row_len,
        row_data: local_row_data.to_vec(),
        overflow_first_page,
    };

    let next = next_leaf(page);
    let pid = page.header().page_id;
    let mut rebuilt = Page::new(PageType::ClusteredLeaf, pid);
    init_clustered_leaf(&mut rebuilt);
    set_next_leaf(&mut rebuilt, next);

    for (idx, cell) in cells.iter().enumerate() {
        match insert_cell_with_overflow(
            &mut rebuilt,
            idx,
            &cell.key,
            &cell.row_header,
            cell.total_row_len,
            &cell.row_data,
            cell.overflow_first_page,
        ) {
            Ok(()) => {}
            Err(DbError::HeapPageFull { .. }) => return Ok(None),
            Err(err) => return Err(err),
        }
    }

    *page = rebuilt;
    Ok(Some(old_image))
}

