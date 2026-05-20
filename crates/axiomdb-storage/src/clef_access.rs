// ── Constants ────────────────────────────────────────────────────────────────

/// Size of the page body (PAGE_SIZE - HEADER_SIZE).
const BODY_SIZE: usize = PAGE_SIZE - HEADER_SIZE;

/// Size of the clustered leaf header within the body.
const CL_HEADER_SIZE: usize = 16;

/// Offset of the cell pointer array within the body.
const CELL_PTR_START: usize = CL_HEADER_SIZE;

/// Size of one cell pointer (body-relative u16 LE offset).
const CELL_PTR_SIZE: usize = 2;

/// Size of the cell metadata (key_len u16 + total_row_len u32).
const CELL_META_SIZE: usize = 6;

/// Size of the RowHeader embedded in each cell.
const ROW_HEADER_SIZE: usize = std::mem::size_of::<RowHeader>();

/// Optional overflow pointer stored at the end of an overflow-backed cell.
const OVERFLOW_PTR_SIZE: usize = std::mem::size_of::<u64>();

/// Minimum freeblock size (next_offset u16 + block_size u16).
const MIN_FREEBLOCK: usize = 4;

/// Sentinel page ID meaning "no next leaf".
pub const NULL_PAGE: u64 = u64::MAX;

/// Maximum primary-key bytes that can fit inline on an otherwise empty
/// clustered leaf page when `row_data` is empty.
pub fn max_inline_key_bytes() -> usize {
    BODY_SIZE - CL_HEADER_SIZE - CELL_PTR_SIZE - CELL_META_SIZE - ROW_HEADER_SIZE
}

/// Maximum primary-key bytes that still leave room for an overflow pointer on a
/// clustered leaf page.
pub fn max_overflow_key_bytes() -> usize {
    max_inline_key_bytes().saturating_sub(OVERFLOW_PTR_SIZE)
}

/// Maximum local row-prefix bytes kept inline for an overflow-backed row with
/// the given primary-key length.
///
/// The budget targets roughly one quarter of the clustered leaf payload area so
/// that large rows still leave room for multiple cells per page.
pub fn max_inline_row_bytes(key_len: usize) -> Option<usize> {
    let overflow_fixed =
        CELL_PTR_SIZE + CELL_META_SIZE + ROW_HEADER_SIZE + key_len + OVERFLOW_PTR_SIZE;
    let quarter_target = page_capacity_bytes() / 4;
    let local_budget = quarter_target.saturating_sub(overflow_fixed);
    if overflow_fixed > page_capacity_bytes() {
        None
    } else {
        Some(local_budget)
    }
}

/// Local row-prefix bytes stored inline for a row with the given logical length.
pub fn local_row_len(key_len: usize, total_row_len: usize) -> usize {
    max_inline_row_bytes(key_len)
        .map(|budget| total_row_len.min(budget))
        .unwrap_or(0)
}

/// Returns whether the logical row requires an overflow-page chain.
pub fn requires_overflow(key_len: usize, total_row_len: usize) -> bool {
    local_row_len(key_len, total_row_len) < total_row_len
}

/// Total on-page footprint of a clustered leaf entry, including its 2-byte
/// pointer-array slot.
pub fn cell_footprint(key_len: usize, total_row_len: usize) -> usize {
    let local_len = local_row_len(key_len, total_row_len);
    let overflow = usize::from(total_row_len > local_len) * OVERFLOW_PTR_SIZE;
    CELL_PTR_SIZE + CELL_META_SIZE + ROW_HEADER_SIZE + key_len + local_len + overflow
}

/// Total bytes available in the body for clustered leaf cells and their
/// pointer-array entries, excluding the fixed page-local header.
pub fn page_capacity_bytes() -> usize {
    BODY_SIZE - CL_HEADER_SIZE
}

/// Returns whether a `(key, row_data)` pair fits on an otherwise empty
/// clustered leaf page.
pub fn fits_inline(key_len: usize, row_len: usize) -> bool {
    cell_footprint(key_len, row_len) <= page_capacity_bytes()
}

// ── Header access ────────────────────────────────────────────────────────────

/// Read `num_cells` from the clustered leaf header.
#[inline]
pub fn num_cells(page: &Page) -> u16 {
    let b = page.as_bytes();
    u16::from_le_bytes([b[HEADER_SIZE + 2], b[HEADER_SIZE + 3]])
}

#[inline]
fn set_num_cells(page: &mut Page, n: u16) {
    let bytes = n.to_le_bytes();
    let b = page.as_bytes_mut();
    b[HEADER_SIZE + 2] = bytes[0];
    b[HEADER_SIZE + 3] = bytes[1];
}

/// Body-relative offset to the lowest cell content.
#[inline]
fn cell_content_start(page: &Page) -> u16 {
    let b = page.as_bytes();
    u16::from_le_bytes([b[HEADER_SIZE + 4], b[HEADER_SIZE + 5]])
}

#[inline]
fn set_cell_content_start(page: &mut Page, v: u16) {
    let bytes = v.to_le_bytes();
    let b = page.as_bytes_mut();
    b[HEADER_SIZE + 4] = bytes[0];
    b[HEADER_SIZE + 5] = bytes[1];
}

/// First freeblock body-relative offset (0 = no freeblocks).
#[inline]
fn freeblock_offset(page: &Page) -> u16 {
    let b = page.as_bytes();
    u16::from_le_bytes([b[HEADER_SIZE + 6], b[HEADER_SIZE + 7]])
}

#[inline]
fn set_freeblock_offset(page: &mut Page, v: u16) {
    let bytes = v.to_le_bytes();
    let b = page.as_bytes_mut();
    b[HEADER_SIZE + 6] = bytes[0];
    b[HEADER_SIZE + 7] = bytes[1];
}

/// Next leaf page ID.
#[inline]
pub fn next_leaf(page: &Page) -> u64 {
    let b = page.as_bytes();
    let off = HEADER_SIZE + 8;
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// Set the next leaf page ID.
#[inline]
pub fn set_next_leaf(page: &mut Page, pid: u64) {
    let bytes = pid.to_le_bytes();
    let off = HEADER_SIZE + 8;
    page.as_bytes_mut()[off..off + 8].copy_from_slice(&bytes);
}

// ── Cell pointer access ──────────────────────────────────────────────────────

/// Read cell pointer at logical index `i` (body-relative offset to cell).
#[inline]
fn cell_ptr_at(page: &Page, i: u16) -> u16 {
    let abs = HEADER_SIZE + CELL_PTR_START + i as usize * CELL_PTR_SIZE;
    let b = page.as_bytes();
    u16::from_le_bytes([b[abs], b[abs + 1]])
}

/// Write cell pointer at logical index `i`.
#[inline]
fn set_cell_ptr_at(page: &mut Page, i: u16, offset: u16) {
    let abs = HEADER_SIZE + CELL_PTR_START + i as usize * CELL_PTR_SIZE;
    let bytes = offset.to_le_bytes();
    let b = page.as_bytes_mut();
    b[abs] = bytes[0];
    b[abs + 1] = bytes[1];
}

/// End of the cell pointer array (body-relative).
#[inline]
fn cell_ptr_array_end(page: &Page) -> usize {
    CELL_PTR_START + num_cells(page) as usize * CELL_PTR_SIZE
}

// ── Cell read ────────────────────────────────────────────────────────────────

/// Total cell size at a given body-relative offset.
#[inline]
fn cell_size_at(page: &Page, body_off: u16) -> usize {
    let abs = HEADER_SIZE + body_off as usize;
    let b = page.as_bytes();
    let key_len = u16::from_le_bytes([b[abs], b[abs + 1]]) as usize;
    let total_row_len =
        u32::from_le_bytes([b[abs + 2], b[abs + 3], b[abs + 4], b[abs + 5]]) as usize;
    let local_len = local_row_len(key_len, total_row_len);
    let overflow = usize::from(total_row_len > local_len) * OVERFLOW_PTR_SIZE;
    CELL_META_SIZE + ROW_HEADER_SIZE + key_len + local_len + overflow
}

/// Read the key bytes from a cell at body-relative offset.
#[inline]
fn cell_key_at(page: &Page, body_off: u16) -> &[u8] {
    let abs = HEADER_SIZE + body_off as usize;
    let b = page.as_bytes();
    let key_len = u16::from_le_bytes([b[abs], b[abs + 1]]) as usize;
    let key_start = abs + CELL_META_SIZE + ROW_HEADER_SIZE;
    &b[key_start..key_start + key_len]
}

/// Parsed cell content returned by [`read_cell`].
pub struct CellRef<'a> {
    pub key: &'a [u8],
    /// Copied from the page (cells may not be 8-byte aligned for bytemuck cast).
    pub row_header: RowHeader,
    pub total_row_len: usize,
    pub row_data: &'a [u8],
    pub overflow_first_page: Option<u64>,
}

#[derive(Debug, Clone)]
struct OwnedCell {
    key: Vec<u8>,
    row_header: RowHeader,
    total_row_len: usize,
    row_data: Vec<u8>,
    overflow_first_page: Option<u64>,
}

/// Attack 17: fast iteration that yields only the `RowHeader` for every
/// cell, skipping key/payload slicing and `CellRef` construction.
///
/// Used by COUNT(*) and other header-only fast paths. The closure
/// receives a reference to a stack-resident `RowHeader` for each cell
/// in logical order. Returns early on bounds errors so callers can
/// surface them via `?`.
#[inline]
pub fn for_each_row_header<F: FnMut(&RowHeader)>(
    page: &Page,
    mut f: F,
) -> Result<(), DbError> {
    let n = num_cells(page);
    let b = page.as_bytes();
    for i in 0..n {
        let ptr_abs = HEADER_SIZE + CELL_PTR_START + i as usize * CELL_PTR_SIZE;
        let body_off = u16::from_le_bytes([b[ptr_abs], b[ptr_abs + 1]]) as usize;
        let hdr_abs = HEADER_SIZE + body_off + CELL_META_SIZE;
        if hdr_abs + ROW_HEADER_SIZE > PAGE_SIZE {
            return Err(DbError::Other(
                "clustered_leaf: row header truncated".into(),
            ));
        }
        // RowHeader is repr(C, packed?) — but cells aren't guaranteed
        // 8-byte aligned, so copy into a stack buffer (cheap, ~8 bytes).
        let mut buf = [0u8; ROW_HEADER_SIZE];
        buf.copy_from_slice(&b[hdr_abs..hdr_abs + ROW_HEADER_SIZE]);
        let hdr: RowHeader = bytemuck::pod_read_unaligned(&buf);
        f(&hdr);
    }
    Ok(())
}

/// Read cell at logical index `idx` (0-based, sorted by key).
pub fn read_cell(page: &Page, idx: u16) -> Result<CellRef<'_>, DbError> {
    let n = num_cells(page);
    if idx >= n {
        return Err(DbError::Other(format!(
            "clustered_leaf: cell index {idx} out of range (num_cells={n})"
        )));
    }
    let body_off = cell_ptr_at(page, idx);
    read_cell_at_offset(page, body_off)
}

/// Read cell at a body-relative offset (internal helper).
///
/// Note: RowHeader requires 8-byte alignment but cells are not guaranteed
/// to be aligned, so we copy into a stack buffer for the header.
fn read_cell_at_offset(page: &Page, body_off: u16) -> Result<CellRef<'_>, DbError> {
    let abs = HEADER_SIZE + body_off as usize;
    let b = page.as_bytes();
    if abs + CELL_META_SIZE + ROW_HEADER_SIZE > PAGE_SIZE {
        return Err(DbError::Other(
            "clustered_leaf: cell header truncated".into(),
        ));
    }
    let key_len = u16::from_le_bytes([b[abs], b[abs + 1]]) as usize;
    let total_row_len =
        u32::from_le_bytes([b[abs + 2], b[abs + 3], b[abs + 4], b[abs + 5]]) as usize;

    let hdr_start = abs + CELL_META_SIZE;
    let key_start = hdr_start + ROW_HEADER_SIZE;
    let row_start = key_start + key_len;
    let local_len = local_row_len(key_len, total_row_len);
    let row_end = row_start + local_len;
    let overflow_end = row_end + usize::from(total_row_len > local_len) * OVERFLOW_PTR_SIZE;

    if overflow_end > PAGE_SIZE {
        return Err(DbError::Other("clustered_leaf: cell data truncated".into()));
    }

    // Copy RowHeader to an aligned stack variable (cells may not be 8-byte aligned).
    let mut hdr_buf = [0u8; ROW_HEADER_SIZE];
    hdr_buf.copy_from_slice(&b[hdr_start..hdr_start + ROW_HEADER_SIZE]);
    let row_header: RowHeader = *bytemuck::from_bytes(&hdr_buf);

    Ok(CellRef {
        key: &b[key_start..key_start + key_len],
        row_header,
        total_row_len,
        row_data: &b[row_start..row_end],
        overflow_first_page: if total_row_len > local_len {
            Some(u64::from_le_bytes(
                b[row_end..overflow_end].try_into().map_err(|_| {
                    DbError::Other("clustered_leaf: overflow pointer truncated".into())
                })?,
            ))
        } else {
            None
        },
    })
}
