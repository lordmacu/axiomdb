//! Clustered index leaf page — stores full row data inline in B-tree leaves.
//!
//! Uses a **SQLite-style cell pointer array** for variable-size cells:
//!
//! ```text
//! [PageHeader: 64B (managed by Page)]
//! Body (16,320 bytes):
//!   [ClusteredLeafHeader: 16B]
//!   [CellPtr 0: 2B][CellPtr 1: 2B]...[CellPtr N-1: 2B]  ← sorted by key
//!                      free space (gap)
//!   [Cell content area: cells in arbitrary order]          ← grows ←
//! ```
//!
//! Each cell:
//! ```text
//! [key_len: u16 LE][row_len: u16 LE][RowHeader: 24B][key_data][row_data]
//! ```
//!
//! The cell pointer array is kept sorted by key, enabling binary search.
//! Cell content is allocated from the end of the page body growing leftward,
//! with a freeblock chain to reclaim space from deleted cells.

use axiomdb_core::error::DbError;

use crate::heap::RowHeader;
use crate::page::{Page, PageType, HEADER_SIZE, PAGE_SIZE};

// ── Constants ────────────────────────────────────────────────────────────────

/// Size of the page body (PAGE_SIZE - HEADER_SIZE).
const BODY_SIZE: usize = PAGE_SIZE - HEADER_SIZE;

/// Size of the clustered leaf header within the body.
const CL_HEADER_SIZE: usize = 16;

/// Offset of the cell pointer array within the body.
const CELL_PTR_START: usize = CL_HEADER_SIZE;

/// Size of one cell pointer (body-relative u16 LE offset).
const CELL_PTR_SIZE: usize = 2;

/// Size of the cell metadata (key_len u16 + row_len u16).
const CELL_META_SIZE: usize = 4;

/// Size of the RowHeader embedded in each cell.
const ROW_HEADER_SIZE: usize = std::mem::size_of::<RowHeader>();

/// Minimum freeblock size (next_offset u16 + block_size u16).
const MIN_FREEBLOCK: usize = 4;

/// Sentinel page ID meaning "no next leaf".
pub const NULL_PAGE: u64 = u64::MAX;

/// Maximum primary-key bytes that can fit inline on an otherwise empty
/// clustered leaf page when `row_data` is empty.
pub fn max_inline_key_bytes() -> usize {
    BODY_SIZE - CL_HEADER_SIZE - CELL_PTR_SIZE - CELL_META_SIZE - ROW_HEADER_SIZE
}

/// Maximum row payload bytes that can fit inline on an otherwise empty
/// clustered leaf page for a given key length.
pub fn max_inline_row_bytes(key_len: usize) -> Option<usize> {
    max_inline_key_bytes().checked_sub(key_len)
}

/// Total on-page footprint of a clustered leaf entry, including its 2-byte
/// pointer-array slot.
pub fn cell_footprint(key_len: usize, row_len: usize) -> usize {
    CELL_PTR_SIZE + CELL_META_SIZE + ROW_HEADER_SIZE + key_len + row_len
}

/// Returns whether a `(key, row_data)` pair fits on an otherwise empty
/// clustered leaf page without overflow support.
pub fn fits_inline(key_len: usize, row_len: usize) -> bool {
    max_inline_row_bytes(key_len).is_some_and(|max| row_len <= max)
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
    let row_len = u16::from_le_bytes([b[abs + 2], b[abs + 3]]) as usize;
    CELL_META_SIZE + ROW_HEADER_SIZE + key_len + row_len
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
    pub row_data: &'a [u8],
}

#[derive(Debug, Clone)]
struct OwnedCell {
    key: Vec<u8>,
    row_header: RowHeader,
    row_data: Vec<u8>,
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
    let row_len = u16::from_le_bytes([b[abs + 2], b[abs + 3]]) as usize;

    let hdr_start = abs + CELL_META_SIZE;
    let key_start = hdr_start + ROW_HEADER_SIZE;
    let row_start = key_start + key_len;
    let cell_end = row_start + row_len;

    if cell_end > PAGE_SIZE {
        return Err(DbError::Other("clustered_leaf: cell data truncated".into()));
    }

    // Copy RowHeader to an aligned stack variable (cells may not be 8-byte aligned).
    let mut hdr_buf = [0u8; ROW_HEADER_SIZE];
    hdr_buf.copy_from_slice(&b[hdr_start..hdr_start + ROW_HEADER_SIZE]);
    let row_header: RowHeader = *bytemuck::from_bytes(&hdr_buf);

    Ok(CellRef {
        key: &b[key_start..key_start + key_len],
        row_header,
        row_data: &b[row_start..cell_end],
    })
}

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
fn total_freeblock_space(page: &Page) -> usize {
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
    let cell_size = CELL_META_SIZE + ROW_HEADER_SIZE + key.len() + row_data.len();
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
    b[abs + 2..abs + 4].copy_from_slice(&(row_data.len() as u16).to_le_bytes());
    b[abs + 4..abs + 4 + ROW_HEADER_SIZE].copy_from_slice(bytemuck::bytes_of(row_header));
    let key_start = abs + CELL_META_SIZE + ROW_HEADER_SIZE;
    b[key_start..key_start + key.len()].copy_from_slice(key);
    let row_start = key_start + key.len();
    b[row_start..row_start + row_data.len()].copy_from_slice(row_data);

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
    let new_image = encode_cell_image(expected_key, new_row_header, new_row_data);
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
        row_data: new_row_data.to_vec(),
    };

    let next = next_leaf(page);
    let pid = page.header().page_id;
    let mut rebuilt = Page::new(PageType::ClusteredLeaf, pid);
    init_clustered_leaf(&mut rebuilt);
    set_next_leaf(&mut rebuilt, next);

    for (idx, cell) in cells.iter().enumerate() {
        match insert_cell(
            &mut rebuilt,
            idx,
            &cell.key,
            &cell.row_header,
            &cell.row_data,
        ) {
            Ok(()) => {}
            Err(DbError::HeapPageFull { .. }) => return Ok(None),
            Err(err) => return Err(err),
        }
    }

    *page = rebuilt;
    Ok(Some(old_image))
}

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

fn encode_cell_image(key: &[u8], row_header: &RowHeader, row_data: &[u8]) -> Vec<u8> {
    let mut image = vec![0u8; CELL_META_SIZE + ROW_HEADER_SIZE + key.len() + row_data.len()];
    image[..2].copy_from_slice(&(key.len() as u16).to_le_bytes());
    image[2..4].copy_from_slice(&(row_data.len() as u16).to_le_bytes());
    image[4..4 + ROW_HEADER_SIZE].copy_from_slice(bytemuck::bytes_of(row_header));
    let key_start = CELL_META_SIZE + ROW_HEADER_SIZE;
    image[key_start..key_start + key.len()].copy_from_slice(key);
    image[key_start + key.len()..].copy_from_slice(row_data);
    image
}

fn collect_cells(page: &Page) -> Result<Vec<OwnedCell>, DbError> {
    let n = num_cells(page) as usize;
    let mut cells = Vec::with_capacity(n);
    for idx in 0..n {
        let cell = read_cell(page, idx as u16)?;
        cells.push(OwnedCell {
            key: cell.key.to_vec(),
            row_header: cell.row_header,
            row_data: cell.row_data.to_vec(),
        });
    }
    Ok(cells)
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::page::Page;

    fn make_page() -> Page {
        let mut page = Page::new(PageType::ClusteredLeaf, 1);
        init_clustered_leaf(&mut page);
        page.update_checksum();
        page
    }

    fn make_row_header(txn_id: u64) -> RowHeader {
        RowHeader {
            txn_id_created: txn_id,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        }
    }

    #[test]
    fn test_init_empty_page() {
        let page = make_page();
        assert_eq!(num_cells(&page), 0);
        assert_eq!(cell_content_start(&page), BODY_SIZE as u16);
        assert_eq!(freeblock_offset(&page), 0);
        assert_eq!(next_leaf(&page), NULL_PAGE);
        assert_eq!(free_space(&page), BODY_SIZE - CL_HEADER_SIZE);
    }

    #[test]
    fn test_insert_one_cell() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        let key = b"key1";
        let data = b"hello world";

        insert_cell(&mut page, 0, key, &hdr, data).unwrap();
        assert_eq!(num_cells(&page), 1);

        let cell = read_cell(&page, 0).unwrap();
        assert_eq!(cell.key, b"key1");
        assert_eq!(cell.row_data, b"hello world");
        assert_eq!(cell.row_header.txn_id_created, 1);
    }

    #[test]
    fn test_insert_sorted_order() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        // Insert keys out of order, but at correct sorted positions.
        // "charlie" first
        let pos = search(&page, b"charlie").unwrap_err();
        insert_cell(&mut page, pos, b"charlie", &hdr, b"c").unwrap();

        // "alpha" before charlie
        let pos = search(&page, b"alpha").unwrap_err();
        insert_cell(&mut page, pos, b"alpha", &hdr, b"a").unwrap();

        // "bravo" between alpha and charlie
        let pos = search(&page, b"bravo").unwrap_err();
        insert_cell(&mut page, pos, b"bravo", &hdr, b"b").unwrap();

        assert_eq!(num_cells(&page), 3);

        // Verify sorted order.
        let c0 = read_cell(&page, 0).unwrap();
        let c1 = read_cell(&page, 1).unwrap();
        let c2 = read_cell(&page, 2).unwrap();
        assert_eq!(c0.key, b"alpha");
        assert_eq!(c1.key, b"bravo");
        assert_eq!(c2.key, b"charlie");
    }

    #[test]
    fn test_rewrite_same_key_same_size_overwrites_in_place() {
        let mut page = make_page();
        let old_hdr = make_row_header(3);
        let new_hdr = make_row_header(9);
        insert_cell(&mut page, 0, b"alpha", &old_hdr, b"hello").unwrap();

        let old_ptr = cell_ptr_at(&page, 0);
        let old_image = rewrite_cell_same_key(&mut page, 0, b"alpha", &new_hdr, b"world").unwrap();

        assert!(old_image.is_some());
        assert_eq!(cell_ptr_at(&page, 0), old_ptr);

        let cell = read_cell(&page, 0).unwrap();
        assert_eq!(cell.key, b"alpha");
        assert_eq!(cell.row_data, b"world");
        assert_eq!(cell.row_header.txn_id_created, 9);
    }

    #[test]
    fn test_rewrite_same_key_growth_rebuilds_same_leaf() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        let new_hdr = make_row_header(7);
        set_next_leaf(&mut page, 777);

        for key in [1u32, 2, 3, 4] {
            let pos = search(&page, &key.to_be_bytes()).unwrap_err();
            insert_cell(
                &mut page,
                pos,
                &key.to_be_bytes(),
                &hdr,
                &vec![key as u8; 400],
            )
            .unwrap();
        }

        let before_free = free_space(&page);
        let old_next = next_leaf(&page);
        let old_num = num_cells(&page);

        let old_image = rewrite_cell_same_key(
            &mut page,
            2,
            &3u32.to_be_bytes(),
            &new_hdr,
            &vec![3u8; 2_000],
        )
        .unwrap();

        assert!(old_image.is_some());
        assert_eq!(next_leaf(&page), old_next);
        assert_eq!(num_cells(&page), old_num);
        assert!(free_space(&page) < before_free);

        let keys: Vec<Vec<u8>> = (0..num_cells(&page))
            .map(|idx| read_cell(&page, idx).unwrap().key.to_vec())
            .collect();
        assert_eq!(
            keys,
            vec![
                1u32.to_be_bytes().to_vec(),
                2u32.to_be_bytes().to_vec(),
                3u32.to_be_bytes().to_vec(),
                4u32.to_be_bytes().to_vec(),
            ]
        );

        let cell = read_cell(&page, 2).unwrap();
        assert_eq!(cell.row_header.txn_id_created, 7);
        assert_eq!(cell.row_data.len(), 2_000);
    }

    #[test]
    fn test_rewrite_same_key_returns_none_when_growth_no_longer_fits() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        let new_hdr = make_row_header(8);

        for key in 0u32..7 {
            let pos = search(&page, &key.to_be_bytes()).unwrap_err();
            insert_cell(
                &mut page,
                pos,
                &key.to_be_bytes(),
                &hdr,
                &vec![key as u8; 2_100],
            )
            .unwrap();
        }

        let before = *page.as_bytes();
        let rewritten = rewrite_cell_same_key(
            &mut page,
            0,
            &0u32.to_be_bytes(),
            &new_hdr,
            &vec![9u8; 8_000],
        )
        .unwrap();

        assert!(rewritten.is_none());
        assert_eq!(page.as_bytes(), &before);
    }

    #[test]
    fn test_search_exact_and_miss() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        for key in [b"aaa" as &[u8], b"ccc", b"eee", b"ggg"] {
            let pos = search(&page, key).unwrap_err();
            insert_cell(&mut page, pos, key, &hdr, b"x").unwrap();
        }

        // Exact matches.
        assert_eq!(search(&page, b"aaa"), Ok(0));
        assert_eq!(search(&page, b"ccc"), Ok(1));
        assert_eq!(search(&page, b"eee"), Ok(2));
        assert_eq!(search(&page, b"ggg"), Ok(3));

        // Misses (insertion positions).
        assert_eq!(search(&page, b"000"), Err(0)); // before all
        assert_eq!(search(&page, b"bbb"), Err(1)); // between aaa and ccc
        assert_eq!(search(&page, b"ddd"), Err(2)); // between ccc and eee
        assert_eq!(search(&page, b"fff"), Err(3)); // between eee and ggg
        assert_eq!(search(&page, b"zzz"), Err(4)); // after all
    }

    #[test]
    fn test_insert_until_full() {
        let mut page = make_page();
        let hdr = make_row_header(1);
        let data = [0u8; 100]; // 100 bytes of row data

        let mut count = 0u32;
        loop {
            let key = count.to_be_bytes();
            let pos = search(&page, &key).unwrap_err();
            match insert_cell(&mut page, pos, &key, &hdr, &data) {
                Ok(()) => count += 1,
                Err(DbError::HeapPageFull { .. }) => break,
                Err(e) => panic!("unexpected error: {e}"),
            }
        }

        assert!(count > 100, "should fit >100 cells, got {count}");
        assert_eq!(num_cells(&page), count as u16);
    }

    #[test]
    fn test_remove_and_reuse() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        // Insert 10 cells.
        for i in 0u32..10 {
            let key = i.to_be_bytes();
            let pos = search(&page, &key).unwrap_err();
            insert_cell(&mut page, pos, &key, &hdr, b"data_here").unwrap();
        }
        assert_eq!(num_cells(&page), 10);
        let space_before = free_space(&page);

        // Remove cell at index 5.
        remove_cell(&mut page, 5).unwrap();
        assert_eq!(num_cells(&page), 9);
        let space_after = free_space(&page);
        assert!(
            space_after > space_before,
            "free space should increase after remove"
        );

        // Insert a new cell — should reuse freed space.
        let new_key = 5u32.to_be_bytes();
        let pos = search(&page, &new_key).unwrap_err();
        insert_cell(&mut page, pos, &new_key, &hdr, b"data_here").unwrap();
        assert_eq!(num_cells(&page), 10);
    }

    #[test]
    fn test_defragment() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        // Insert 20 cells.
        for i in 0u32..20 {
            let key = i.to_be_bytes();
            let pos = search(&page, &key).unwrap_err();
            insert_cell(&mut page, pos, &key, &hdr, b"test_data_here!!").unwrap();
        }

        // Remove every other cell (creates fragmentation).
        for i in (0..10).rev() {
            remove_cell(&mut page, i * 2).unwrap();
        }
        assert_eq!(num_cells(&page), 10);

        let space_before_defrag = free_space(&page);
        let gap_before = gap_space(&page);

        // Defragment.
        defragment(&mut page);

        let space_after = free_space(&page);
        let gap_after = gap_space(&page);

        // After defrag, all free space should be contiguous (gap = total free).
        assert_eq!(freeblock_offset(&page), 0, "no freeblocks after defrag");
        assert_eq!(gap_after, space_after, "all free space is gap after defrag");
        assert!(gap_after >= gap_before, "gap should not shrink");
        // Total free space is preserved (no data lost).
        assert_eq!(space_after, space_before_defrag);

        // Verify all remaining cells are intact and in order.
        for i in 0..10u16 {
            let cell = read_cell(&page, i).unwrap();
            let expected_key = ((i as u32) * 2 + 1).to_be_bytes();
            assert_eq!(
                cell.key, &expected_key,
                "cell {i} key mismatch after defrag"
            );
            assert_eq!(cell.row_data, b"test_data_here!!");
        }
    }

    #[test]
    fn test_next_leaf_chain() {
        let mut page = make_page();
        assert_eq!(next_leaf(&page), NULL_PAGE);

        set_next_leaf(&mut page, 42);
        assert_eq!(next_leaf(&page), 42);

        set_next_leaf(&mut page, NULL_PAGE);
        assert_eq!(next_leaf(&page), NULL_PAGE);
    }

    #[test]
    fn test_mvcc_visibility() {
        let mut page = make_page();

        // Insert a live cell (txn_id_deleted = 0).
        let hdr_live = RowHeader {
            txn_id_created: 10,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        };
        insert_cell(&mut page, 0, b"live", &hdr_live, b"data").unwrap();

        // Insert a deleted cell (txn_id_deleted = 20).
        let hdr_dead = RowHeader {
            txn_id_created: 10,
            txn_id_deleted: 20,
            row_version: 1,
            _flags: 0,
        };
        let pos = search(&page, b"dead").unwrap_err();
        insert_cell(&mut page, pos, b"dead", &hdr_dead, b"gone").unwrap();

        // Read both cells and check MVCC fields.
        let live = read_cell(&page, search(&page, b"live").unwrap() as u16).unwrap();
        assert_eq!(live.row_header.txn_id_deleted, 0);

        let dead = read_cell(&page, search(&page, b"dead").unwrap() as u16).unwrap();
        assert_eq!(dead.row_header.txn_id_deleted, 20);
    }

    #[test]
    fn test_many_inserts_and_removes_stress() {
        let mut page = make_page();
        let hdr = make_row_header(1);

        // Insert 50 cells.
        for i in 0u32..50 {
            let key = format!("{i:08}");
            let pos = search(&page, key.as_bytes()).unwrap_err();
            insert_cell(&mut page, pos, key.as_bytes(), &hdr, b"value").unwrap();
        }
        assert_eq!(num_cells(&page), 50);

        // Remove 25 cells.
        for i in (0..25).rev() {
            remove_cell(&mut page, i * 2).unwrap();
        }
        assert_eq!(num_cells(&page), 25);

        // Defragment.
        defragment(&mut page);

        // Insert 25 more cells.
        for i in 50u32..75 {
            let key = format!("{i:08}");
            let pos = search(&page, key.as_bytes()).unwrap_err();
            insert_cell(&mut page, pos, key.as_bytes(), &hdr, b"value").unwrap();
        }
        assert_eq!(num_cells(&page), 50);

        // Verify sorted order.
        for i in 0..49 {
            let c1 = read_cell(&page, i).unwrap();
            let c2 = read_cell(&page, i + 1).unwrap();
            assert!(c1.key < c2.key, "cells not sorted at {i}");
        }
    }
}
