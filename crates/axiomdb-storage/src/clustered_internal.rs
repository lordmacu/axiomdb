//! Clustered index internal page — variable-size separator keys plus logical
//! child pointers in a slotted-page layout.
//!
//! Layout:
//!
//! ```text
//! [PageHeader: 64B (managed by Page)]
//! Body (16,320 bytes):
//!   [ClusteredInternalHeader: 16B]
//!     is_leaf: u8 = 0
//!     _pad0: u8
//!     num_cells: u16
//!     cell_content_start: u16
//!     freeblock_offset: u16
//!     leftmost_child: u64
//!   [CellPtr 0: 2B][CellPtr 1: 2B]...[CellPtr N-1: 2B]  <- sorted by key
//!                      free space (gap)
//!   [Cell content area: cells in arbitrary order]       <- grows <-
//! ```
//!
//! Each separator cell:
//!
//! ```text
//! [right_child: u64 LE][key_len: u16 LE][key_data]
//! ```
//!
//! Child mapping:
//! - logical child `0` = `leftmost_child` from the header
//! - logical child `i > 0` = `right_child` of separator cell `i - 1`

use axiomdb_core::error::DbError;

use crate::page::{Page, PageType, HEADER_SIZE, PAGE_SIZE};

const BODY_SIZE: usize = PAGE_SIZE - HEADER_SIZE;
const CI_HEADER_SIZE: usize = 16;
const CELL_PTR_START: usize = CI_HEADER_SIZE;
const CELL_PTR_SIZE: usize = 2;
const CELL_META_SIZE: usize = 10;
const MIN_FREEBLOCK: usize = 4;

/// Sentinel page identifier used when an internal page does not yet have a
/// valid child pointer assigned.
pub const NULL_PAGE: u64 = u64::MAX;

pub struct CellRef<'a> {
    pub key: &'a [u8],
    pub right_child: u64,
}

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

#[inline]
pub fn leftmost_child(page: &Page) -> u64 {
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

#[inline]
pub fn set_leftmost_child(page: &mut Page, pid: u64) {
    let bytes = pid.to_le_bytes();
    let off = HEADER_SIZE + 8;
    page.as_bytes_mut()[off..off + 8].copy_from_slice(&bytes);
}

#[inline]
fn cell_ptr_at(page: &Page, i: u16) -> u16 {
    let abs = HEADER_SIZE + CELL_PTR_START + i as usize * CELL_PTR_SIZE;
    let b = page.as_bytes();
    u16::from_le_bytes([b[abs], b[abs + 1]])
}

#[inline]
fn set_cell_ptr_at(page: &mut Page, i: u16, offset: u16) {
    let abs = HEADER_SIZE + CELL_PTR_START + i as usize * CELL_PTR_SIZE;
    let bytes = offset.to_le_bytes();
    let b = page.as_bytes_mut();
    b[abs] = bytes[0];
    b[abs + 1] = bytes[1];
}

#[inline]
fn cell_ptr_array_end(page: &Page) -> usize {
    CELL_PTR_START + num_cells(page) as usize * CELL_PTR_SIZE
}

#[inline]
fn gap_space(page: &Page) -> usize {
    let ptr_end = cell_ptr_array_end(page);
    let content_start = cell_content_start(page) as usize;
    content_start.saturating_sub(ptr_end)
}

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

pub fn free_space(page: &Page) -> usize {
    gap_space(page) + total_freeblock_space(page)
}

#[inline]
fn cell_size_at(page: &Page, body_off: u16) -> usize {
    let abs = HEADER_SIZE + body_off as usize;
    let b = page.as_bytes();
    let key_len = u16::from_le_bytes([b[abs + 8], b[abs + 9]]) as usize;
    CELL_META_SIZE + key_len
}

#[inline]
fn cell_right_child_at(page: &Page, body_off: u16) -> u64 {
    let abs = HEADER_SIZE + body_off as usize;
    let b = page.as_bytes();
    u64::from_le_bytes([
        b[abs],
        b[abs + 1],
        b[abs + 2],
        b[abs + 3],
        b[abs + 4],
        b[abs + 5],
        b[abs + 6],
        b[abs + 7],
    ])
}

#[inline]
fn cell_key_at(page: &Page, body_off: u16) -> &[u8] {
    let abs = HEADER_SIZE + body_off as usize;
    let b = page.as_bytes();
    let key_len = u16::from_le_bytes([b[abs + 8], b[abs + 9]]) as usize;
    let key_start = abs + CELL_META_SIZE;
    &b[key_start..key_start + key_len]
}

fn allocate_from_freeblocks(page: &mut Page, size: usize) -> Option<u16> {
    let mut prev_off: Option<u16> = None;
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
                let new_fb_off = fb_off + size as u16;
                let new_abs = HEADER_SIZE + new_fb_off as usize;
                let b = page.as_bytes_mut();
                b[new_abs..new_abs + 2].copy_from_slice(&next.to_le_bytes());
                b[new_abs + 2..new_abs + 4].copy_from_slice(&(remainder as u16).to_le_bytes());
                if let Some(prev) = prev_off {
                    let prev_abs = HEADER_SIZE + prev as usize;
                    b[prev_abs..prev_abs + 2].copy_from_slice(&new_fb_off.to_le_bytes());
                } else {
                    set_freeblock_offset(page, new_fb_off);
                }
            } else if let Some(prev) = prev_off {
                let b = page.as_bytes_mut();
                let prev_abs = HEADER_SIZE + prev as usize;
                b[prev_abs..prev_abs + 2].copy_from_slice(&next.to_le_bytes());
            } else {
                set_freeblock_offset(page, next);
            }
            return Some(fb_off);
        }

        prev_off = Some(fb_off);
        fb_off = next;
    }

    None
}

pub fn init_clustered_internal(page: &mut Page, left_child: u64) {
    page.header_mut().page_type = PageType::ClusteredInternal as u8;

    let b = page.as_bytes_mut();
    b[HEADER_SIZE] = 0;
    b[HEADER_SIZE + 1] = 0;

    set_num_cells(page, 0);
    set_cell_content_start(page, BODY_SIZE as u16);
    set_freeblock_offset(page, 0);
    set_leftmost_child(page, left_child);
}

pub fn read_cell(page: &Page, idx: u16) -> Result<CellRef<'_>, DbError> {
    let n = num_cells(page);
    if idx >= n {
        return Err(DbError::Other(format!(
            "clustered_internal: cell index {idx} out of range (num_cells={n})"
        )));
    }

    let body_off = cell_ptr_at(page, idx);
    let abs = HEADER_SIZE + body_off as usize;
    if abs + CELL_META_SIZE > PAGE_SIZE {
        return Err(DbError::Other(
            "clustered_internal: cell header truncated".into(),
        ));
    }

    let key_len = u16::from_le_bytes([page.as_bytes()[abs + 8], page.as_bytes()[abs + 9]]) as usize;
    let key_start = abs + CELL_META_SIZE;
    let cell_end = key_start + key_len;
    if cell_end > PAGE_SIZE {
        return Err(DbError::Other(
            "clustered_internal: cell data truncated".into(),
        ));
    }

    Ok(CellRef {
        key: &page.as_bytes()[key_start..cell_end],
        right_child: cell_right_child_at(page, body_off),
    })
}

pub fn key_at(page: &Page, idx: u16) -> Result<&[u8], DbError> {
    let n = num_cells(page);
    if idx >= n {
        return Err(DbError::Other(format!(
            "clustered_internal: cell index {idx} out of range (num_cells={n})"
        )));
    }

    let body_off = cell_ptr_at(page, idx);
    let abs = HEADER_SIZE + body_off as usize;
    if abs + CELL_META_SIZE > PAGE_SIZE {
        return Err(DbError::Other(
            "clustered_internal: cell header truncated".into(),
        ));
    }

    let key_len = u16::from_le_bytes([page.as_bytes()[abs + 8], page.as_bytes()[abs + 9]]) as usize;
    let key_start = abs + CELL_META_SIZE;
    let cell_end = key_start + key_len;
    if cell_end > PAGE_SIZE {
        return Err(DbError::Other(
            "clustered_internal: cell data truncated".into(),
        ));
    }

    Ok(cell_key_at(page, body_off))
}

pub fn child_at(page: &Page, idx: u16) -> Result<u64, DbError> {
    let n = num_cells(page);
    if idx > n {
        return Err(DbError::Other(format!(
            "clustered_internal: child index {idx} out of range (num_cells={n})"
        )));
    }

    if idx == 0 {
        Ok(leftmost_child(page))
    } else {
        let body_off = cell_ptr_at(page, idx - 1);
        Ok(cell_right_child_at(page, body_off))
    }
}

pub fn set_child_at(page: &mut Page, idx: u16, pid: u64) -> Result<(), DbError> {
    let n = num_cells(page);
    if idx > n {
        return Err(DbError::Other(format!(
            "clustered_internal: child index {idx} out of range (num_cells={n})"
        )));
    }

    if idx == 0 {
        set_leftmost_child(page, pid);
        return Ok(());
    }

    let body_off = cell_ptr_at(page, idx - 1);
    let abs = HEADER_SIZE + body_off as usize;
    if abs + 8 > PAGE_SIZE {
        return Err(DbError::Other(
            "clustered_internal: child field truncated".into(),
        ));
    }

    page.as_bytes_mut()[abs..abs + 8].copy_from_slice(&pid.to_le_bytes());
    Ok(())
}

pub fn find_child_idx(page: &Page, key: &[u8]) -> Result<usize, DbError> {
    let n = num_cells(page) as usize;
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let mid_key = key_at(page, mid as u16)?;
        if mid_key <= key {
            lo = mid + 1;
        } else {
            hi = mid;
        }
    }
    Ok(lo)
}

pub fn insert_at(
    page: &mut Page,
    pos: usize,
    sep_key: &[u8],
    right_child: u64,
) -> Result<(), DbError> {
    let n = num_cells(page) as usize;
    if pos > n {
        return Err(DbError::Other(format!(
            "clustered_internal: insert pos {pos} > num_cells {n}"
        )));
    }

    let cell_size = CELL_META_SIZE + sep_key.len();
    let need_gap = CELL_PTR_SIZE;

    let cell_offset = if let Some(fb_off) = allocate_from_freeblocks(page, cell_size) {
        if gap_space(page) < need_gap {
            return Err(DbError::HeapPageFull {
                page_id: page.header().page_id,
                needed: need_gap,
                available: gap_space(page),
            });
        }
        fb_off
    } else {
        let total_need = cell_size + need_gap;
        let gap = gap_space(page);
        if gap < total_need {
            return Err(DbError::HeapPageFull {
                page_id: page.header().page_id,
                needed: total_need,
                available: gap,
            });
        }
        let new_start = cell_content_start(page) as usize - cell_size;
        set_cell_content_start(page, new_start as u16);
        new_start as u16
    };

    let abs = HEADER_SIZE + cell_offset as usize;
    let b = page.as_bytes_mut();
    b[abs..abs + 8].copy_from_slice(&right_child.to_le_bytes());
    b[abs + 8..abs + 10].copy_from_slice(&(sep_key.len() as u16).to_le_bytes());
    let key_start = abs + CELL_META_SIZE;
    b[key_start..key_start + sep_key.len()].copy_from_slice(sep_key);

    let ptr_base = HEADER_SIZE + CELL_PTR_START;
    let src = ptr_base + pos * CELL_PTR_SIZE;
    let dst = src + CELL_PTR_SIZE;
    let count = (n - pos) * CELL_PTR_SIZE;
    if count > 0 {
        page.as_bytes_mut().copy_within(src..src + count, dst);
    }

    set_cell_ptr_at(page, pos as u16, cell_offset);
    set_num_cells(page, (n + 1) as u16);
    Ok(())
}

pub fn remove_at(page: &mut Page, key_pos: usize, child_pos: usize) -> Result<(), DbError> {
    let n = num_cells(page) as usize;
    if n == 0 {
        return Err(DbError::Other(
            "clustered_internal: cannot remove from empty page".into(),
        ));
    }
    if key_pos >= n {
        return Err(DbError::Other(format!(
            "clustered_internal: key pos {key_pos} >= num_cells {n}"
        )));
    }
    if child_pos > n {
        return Err(DbError::Other(format!(
            "clustered_internal: child pos {child_pos} > num_cells {n}"
        )));
    }
    if child_pos != key_pos && child_pos != key_pos + 1 {
        return Err(DbError::Other(format!(
            "clustered_internal: child pos {child_pos} is not adjacent to key pos {key_pos}"
        )));
    }

    let removed_off = cell_ptr_at(page, key_pos as u16);
    let removed_size = cell_size_at(page, removed_off);
    let replacement_child = cell_right_child_at(page, removed_off);

    if child_pos == key_pos {
        if key_pos == 0 {
            set_leftmost_child(page, replacement_child);
        } else {
            set_child_at(page, key_pos as u16, replacement_child)?;
        }
    }

    if removed_size >= MIN_FREEBLOCK {
        let old_head = freeblock_offset(page);
        let abs = HEADER_SIZE + removed_off as usize;
        let b = page.as_bytes_mut();
        b[abs..abs + 2].copy_from_slice(&old_head.to_le_bytes());
        b[abs + 2..abs + 4].copy_from_slice(&(removed_size as u16).to_le_bytes());
        set_freeblock_offset(page, removed_off);
    }

    let ptr_base = HEADER_SIZE + CELL_PTR_START;
    let dst = ptr_base + key_pos * CELL_PTR_SIZE;
    let src = dst + CELL_PTR_SIZE;
    let count = (n - 1 - key_pos) * CELL_PTR_SIZE;
    if count > 0 {
        page.as_bytes_mut().copy_within(src..src + count, dst);
    }

    set_num_cells(page, (n - 1) as u16);
    Ok(())
}

pub fn defragment(page: &mut Page) {
    let n = num_cells(page) as usize;
    if n == 0 {
        set_cell_content_start(page, BODY_SIZE as u16);
        set_freeblock_offset(page, 0);
        return;
    }

    let mut cell_data: Vec<(usize, Vec<u8>)> = Vec::with_capacity(n);
    for i in 0..n {
        let off = cell_ptr_at(page, i as u16);
        let size = cell_size_at(page, off);
        let abs = HEADER_SIZE + off as usize;
        cell_data.push((i, page.as_bytes()[abs..abs + size].to_vec()));
    }

    let mut write_pos = BODY_SIZE;
    let mut new_offsets = vec![0u16; n];
    for &(idx, ref data) in cell_data.iter().rev() {
        write_pos -= data.len();
        let dst_abs = HEADER_SIZE + write_pos;
        page.as_bytes_mut()[dst_abs..dst_abs + data.len()].copy_from_slice(data);
        new_offsets[idx] = write_pos as u16;
    }

    for (i, &off) in new_offsets.iter().enumerate() {
        set_cell_ptr_at(page, i as u16, off);
    }

    set_cell_content_start(page, write_pos as u16);
    set_freeblock_offset(page, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_page(left_child: u64) -> Page {
        let mut page = Page::new(PageType::ClusteredInternal, 7);
        init_clustered_internal(&mut page, left_child);
        page.update_checksum();
        page
    }

    #[test]
    fn test_init_empty_page() {
        let page = make_page(11);
        assert_eq!(num_cells(&page), 0);
        assert_eq!(leftmost_child(&page), 11);
        assert_eq!(cell_content_start(&page), BODY_SIZE as u16);
        assert_eq!(freeblock_offset(&page), 0);
        assert_eq!(free_space(&page), BODY_SIZE - CI_HEADER_SIZE);
    }

    #[test]
    fn test_insert_and_child_mapping() {
        let mut page = make_page(10);

        insert_at(&mut page, 0, b"bravo", 20).unwrap();
        insert_at(&mut page, 0, b"alpha", 15).unwrap();
        insert_at(&mut page, 2, b"charlie_longer", 30).unwrap();

        assert_eq!(num_cells(&page), 3);
        assert_eq!(key_at(&page, 0).unwrap(), b"alpha");
        assert_eq!(key_at(&page, 1).unwrap(), b"bravo");
        assert_eq!(key_at(&page, 2).unwrap(), b"charlie_longer");

        assert_eq!(child_at(&page, 0).unwrap(), 10);
        assert_eq!(child_at(&page, 1).unwrap(), 15);
        assert_eq!(child_at(&page, 2).unwrap(), 20);
        assert_eq!(child_at(&page, 3).unwrap(), 30);
    }

    #[test]
    fn test_find_child_idx_semantics() {
        let mut page = make_page(10);
        insert_at(&mut page, 0, b"bbb", 20).unwrap();
        insert_at(&mut page, 1, b"ddd", 30).unwrap();

        assert_eq!(find_child_idx(&page, b"aaa").unwrap(), 0);
        assert_eq!(find_child_idx(&page, b"bbb").unwrap(), 1);
        assert_eq!(find_child_idx(&page, b"ccc").unwrap(), 1);
        assert_eq!(find_child_idx(&page, b"ddd").unwrap(), 2);
        assert_eq!(find_child_idx(&page, b"zzz").unwrap(), 2);
    }

    #[test]
    fn test_set_child_at_updates_leftmost_and_right_children() {
        let mut page = make_page(10);
        insert_at(&mut page, 0, b"bbb", 20).unwrap();
        insert_at(&mut page, 1, b"ddd", 30).unwrap();

        set_child_at(&mut page, 0, 111).unwrap();
        set_child_at(&mut page, 2, 333).unwrap();

        assert_eq!(child_at(&page, 0).unwrap(), 111);
        assert_eq!(child_at(&page, 1).unwrap(), 20);
        assert_eq!(child_at(&page, 2).unwrap(), 333);
    }

    #[test]
    fn test_remove_right_child_side() {
        let mut page = make_page(10);
        insert_at(&mut page, 0, b"bbb", 20).unwrap();
        insert_at(&mut page, 1, b"ddd", 30).unwrap();
        insert_at(&mut page, 2, b"fff", 40).unwrap();

        remove_at(&mut page, 1, 2).unwrap();

        assert_eq!(num_cells(&page), 2);
        assert_eq!(key_at(&page, 0).unwrap(), b"bbb");
        assert_eq!(key_at(&page, 1).unwrap(), b"fff");
        assert_eq!(child_at(&page, 0).unwrap(), 10);
        assert_eq!(child_at(&page, 1).unwrap(), 20);
        assert_eq!(child_at(&page, 2).unwrap(), 40);
    }

    #[test]
    fn test_remove_left_child_side_updates_leftmost() {
        let mut page = make_page(10);
        insert_at(&mut page, 0, b"bbb", 20).unwrap();
        insert_at(&mut page, 1, b"ddd", 30).unwrap();

        remove_at(&mut page, 0, 0).unwrap();

        assert_eq!(num_cells(&page), 1);
        assert_eq!(leftmost_child(&page), 20);
        assert_eq!(key_at(&page, 0).unwrap(), b"ddd");
        assert_eq!(child_at(&page, 0).unwrap(), 20);
        assert_eq!(child_at(&page, 1).unwrap(), 30);
    }

    #[test]
    fn test_remove_left_child_side_updates_previous_separator() {
        let mut page = make_page(10);
        insert_at(&mut page, 0, b"bbb", 20).unwrap();
        insert_at(&mut page, 1, b"ddd", 30).unwrap();
        insert_at(&mut page, 2, b"fff", 40).unwrap();

        remove_at(&mut page, 1, 1).unwrap();

        assert_eq!(num_cells(&page), 2);
        assert_eq!(key_at(&page, 0).unwrap(), b"bbb");
        assert_eq!(key_at(&page, 1).unwrap(), b"fff");
        assert_eq!(child_at(&page, 0).unwrap(), 10);
        assert_eq!(child_at(&page, 1).unwrap(), 30);
        assert_eq!(child_at(&page, 2).unwrap(), 40);
    }

    #[test]
    fn test_defragment_preserves_keys_and_children() {
        let mut page = make_page(100);
        for (idx, (key, child)) in [
            (b"alpha".as_slice(), 110u64),
            (b"bravo_bigger".as_slice(), 120),
            (b"charlie_even_bigger".as_slice(), 130),
            (b"delta".as_slice(), 140),
            (b"echo_super_long_separator".as_slice(), 150),
        ]
        .iter()
        .enumerate()
        {
            insert_at(&mut page, idx, key, *child).unwrap();
        }

        remove_at(&mut page, 1, 2).unwrap();
        remove_at(&mut page, 2, 3).unwrap();

        let free_before = free_space(&page);
        let gap_before = gap_space(&page);

        defragment(&mut page);

        assert_eq!(freeblock_offset(&page), 0);
        assert_eq!(free_space(&page), free_before);
        assert!(gap_space(&page) >= gap_before);

        assert_eq!(key_at(&page, 0).unwrap(), b"alpha");
        assert_eq!(key_at(&page, 1).unwrap(), b"charlie_even_bigger");
        assert_eq!(key_at(&page, 2).unwrap(), b"echo_super_long_separator");
        assert_eq!(child_at(&page, 0).unwrap(), 100);
        assert_eq!(child_at(&page, 1).unwrap(), 110);
        assert_eq!(child_at(&page, 2).unwrap(), 130);
        assert_eq!(child_at(&page, 3).unwrap(), 150);
    }

    #[test]
    fn test_page_full_and_bounds_errors() {
        let mut page = make_page(1);

        let huge_key = vec![b'x'; BODY_SIZE / 2];
        insert_at(&mut page, 0, &huge_key, 2).unwrap();

        let err = insert_at(&mut page, 1, &huge_key, 3).unwrap_err();
        assert!(matches!(err, DbError::HeapPageFull { .. }));

        assert!(key_at(&page, 9).is_err());
        assert!(child_at(&page, 9).is_err());
        assert!(set_child_at(&mut page, 9, 10).is_err());
        assert!(remove_at(&mut page, 0, 9).is_err());
        assert!(remove_at(&mut page, 0, 0).is_ok());
    }
}
