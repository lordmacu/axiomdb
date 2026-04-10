use super::OwnedInternalCell;
use super::OwnedLeafCell;
use crate::{
    clustered_internal, clustered_leaf, clustered_overflow,
    heap::RowHeader,
    page::{Page, PageType},
    StorageEngine,
};
use axiomdb_core::error::DbError;
use std::ops::Bound;

pub(super) fn collect_leaf_cells(page: &Page) -> Result<Vec<OwnedLeafCell>, DbError> {
    let num_cells = clustered_leaf::num_cells(page);
    let mut cells = Vec::with_capacity(num_cells as usize);
    for idx in 0..num_cells {
        let cell = clustered_leaf::read_cell(page, idx)?;
        cells.push(OwnedLeafCell {
            key: cell.key.to_vec(),
            row_header: cell.row_header,
            total_row_len: cell.total_row_len,
            local_row_data: cell.row_data.to_vec(),
            overflow_first_page: cell.overflow_first_page,
        });
    }
    Ok(cells)
}

pub(super) fn collect_internal_cells(page: &Page) -> Result<Vec<OwnedInternalCell>, DbError> {
    let num_cells = clustered_internal::num_cells(page);
    let mut cells = Vec::with_capacity(num_cells as usize);
    for idx in 0..num_cells {
        let cell = clustered_internal::read_cell(page, idx)?;
        cells.push(OwnedInternalCell {
            key: cell.key.to_vec(),
            right_child: cell.right_child,
        });
    }
    Ok(cells)
}

pub(super) fn rebuild_leaf_page(
    page: &mut Page,
    cells: &[OwnedLeafCell],
    next_leaf_pid: u64,
) -> Result<(), DbError> {
    *page = Page::new(PageType::ClusteredLeaf, page.header().page_id);
    clustered_leaf::init_clustered_leaf(page);
    clustered_leaf::set_next_leaf(page, next_leaf_pid);
    for (idx, cell) in cells.iter().enumerate() {
        clustered_leaf::insert_cell_with_overflow(
            page,
            idx,
            &cell.key,
            &cell.row_header,
            cell.total_row_len,
            &cell.local_row_data,
            cell.overflow_first_page,
        )?;
    }
    Ok(())
}

pub(super) fn rebuild_internal_page(
    page: &mut Page,
    leftmost_child: u64,
    separators: &[OwnedInternalCell],
) -> Result<(), DbError> {
    *page = Page::new(PageType::ClusteredInternal, page.header().page_id);
    clustered_internal::init_clustered_internal(page, leftmost_child);
    for (idx, cell) in separators.iter().enumerate() {
        clustered_internal::insert_at(page, idx, &cell.key, cell.right_child)?;
    }
    Ok(())
}

pub(super) fn leaf_footprint(cells: &[OwnedLeafCell]) -> usize {
    cells
        .iter()
        .map(|cell| clustered_leaf::cell_footprint(cell.key.len(), cell.total_row_len))
        .sum()
}

pub(super) fn internal_footprint(separators: &[OwnedInternalCell]) -> usize {
    separators
        .iter()
        .map(|cell| clustered_internal::separator_footprint(cell.key.len()))
        .sum()
}

pub(super) fn leaf_underfull(page: &Page) -> bool {
    let used =
        clustered_leaf::page_capacity_bytes().saturating_sub(clustered_leaf::free_space(page));
    used * 4 < clustered_leaf::page_capacity_bytes()
}

pub(super) fn internal_underfull(page: &Page) -> bool {
    let used = clustered_internal::page_capacity_bytes()
        .saturating_sub(clustered_internal::free_space(page));
    used * 4 < clustered_internal::page_capacity_bytes()
}

/// Phase 40.8c: returns `true` when the descent into `child_pid` from a
/// pessimistic insert is guaranteed not to require an update on the parent
/// of `child_pid` (no upward split propagation).
///
/// The clustered B-tree never changes a page's pid on split (the old page
/// stays as the left half) so the only reason a parent latch must outlive
/// the recursion is to absorb a propagated separator. The check verifies
/// that the immediate child has at least the worst-case footprint of the
/// new entry available in `free_space`, doubled as headroom to cover the
/// fact that the propagated separator might be slightly larger than the
/// inserted key (the median promotion picks among existing leaf keys).
pub(super) fn child_is_safe_for_insert(
    storage: &dyn StorageEngine,
    child_pid: u64,
    key: &[u8],
    row_data_len: usize,
) -> Result<bool, DbError> {
    let page = storage.read_page(child_pid)?;
    match clustered_page_type(&page)? {
        PageType::ClusteredLeaf => {
            let needed = clustered_leaf::cell_footprint(key.len(), row_data_len);
            // Require 2× headroom so the in-place fast path stays viable even
            // when subsequent inserts to this leaf approach the byte limit.
            Ok(clustered_leaf::free_space(&page) >= needed.saturating_mul(2))
        }
        PageType::ClusteredInternal => {
            // Internal child must absorb a propagated separator, whose key
            // bytes are bounded above by the longest key in its subtree. We
            // approximate that with the inserted key length plus a generous
            // margin and then double the requirement so that even an
            // unexpectedly large separator (or a follow-up split) does not
            // turn this descent into a parent rewrite.
            let estimated_sep_len = key.len().saturating_add(64);
            let needed = clustered_internal::separator_footprint(estimated_sep_len);
            Ok(clustered_internal::free_space(&page) >= needed.saturating_mul(2))
        }
        other => Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered tree safety check encountered unsupported page type {other:?} at page {child_pid}"
            ),
        }),
    }
}

/// Phase 40.8c: returns `true` when the descent into `child_pid` from a
/// pessimistic delete is guaranteed not to require an update on the parent
/// of `child_pid` (no upward rebalance propagation).
///
/// In the clustered B-tree, leaves and internals never change pid on
/// rebalance/merge (left page is reused). The only thing that propagates
/// upward is `underfull = true`, which triggers `rebalance_child` on the
/// parent. To make sure the recursion does not surface that signal, we
/// require the child to be **comfortably** above the underfull byte
/// threshold (used > capacity/2 for both leaf and internal pages).
pub(super) fn child_is_safe_for_delete(
    storage: &dyn StorageEngine,
    child_pid: u64,
) -> Result<bool, DbError> {
    let page = storage.read_page(child_pid)?;
    match clustered_page_type(&page)? {
        PageType::ClusteredLeaf => {
            let cap = clustered_leaf::page_capacity_bytes();
            let used = cap.saturating_sub(clustered_leaf::free_space(&page));
            // Underfull is `used * 4 < cap`, i.e., `used < cap/4`.
            // Require `used > cap/2` so even after losing one cell the page
            // remains comfortably above the underfull threshold.
            Ok(used.saturating_mul(2) > cap)
        }
        PageType::ClusteredInternal => {
            // Internal nodes also rebalance based on byte occupancy, but the
            // safety analysis must additionally rule out a deeper underflow
            // surfacing back into this child. Restricting early release to
            // **leaf** children avoids walking the subtree while still
            // covering the most common bottom-of-tree descent pattern.
            Ok(false)
        }
        other => Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered tree delete safety check encountered unsupported page type {other:?} at page {child_pid}"
            ),
        }),
    }
}

pub(super) fn choose_leaf_split_idx(cells: &[OwnedLeafCell]) -> usize {
    debug_assert!(cells.len() >= 2);
    let footprints: Vec<usize> = cells
        .iter()
        .map(|cell| clustered_leaf::cell_footprint(cell.key.len(), cell.total_row_len))
        .collect();
    choose_balanced_boundary(&footprints)
}

pub(super) fn choose_leaf_rebalance_idx(cells: &[OwnedLeafCell]) -> Result<usize, DbError> {
    debug_assert!(cells.len() >= 2);
    let cap = clustered_leaf::page_capacity_bytes();
    let footprints: Vec<usize> = cells
        .iter()
        .map(|cell| clustered_leaf::cell_footprint(cell.key.len(), cell.total_row_len))
        .collect();
    let total: usize = footprints.iter().sum();
    let mut left = 0usize;
    let mut best_idx = None;
    let mut best_diff = usize::MAX;

    for split_at in 1..footprints.len() {
        left += footprints[split_at - 1];
        let right = total - left;
        if left > cap || right > cap {
            continue;
        }

        let diff = left.abs_diff(right);
        if diff < best_diff {
            best_diff = diff;
            best_idx = Some(split_at);
        }
    }

    best_idx.ok_or_else(|| DbError::BTreeCorrupted {
        msg: "clustered leaf rebalance found no byte-balanced split boundary".into(),
    })
}

pub(super) fn choose_internal_promotion_idx(separators: &[OwnedInternalCell]) -> usize {
    debug_assert!(!separators.is_empty());

    if separators.len() == 1 {
        return 0;
    }

    let footprints: Vec<usize> = separators
        .iter()
        .map(|cell| clustered_internal::separator_footprint(cell.key.len()))
        .collect();
    let total: usize = footprints.iter().sum();
    let mut prefix = 0usize;
    let mut best_idx = 0usize;
    let mut best_diff = usize::MAX;

    let allow_edge = separators.len() <= 2;
    for (mid, footprint) in footprints.iter().copied().enumerate() {
        if !allow_edge && (mid == 0 || mid + 1 == separators.len()) {
            prefix += footprint;
            continue;
        }

        let left = prefix;
        let right = total - prefix - footprint;
        let diff = left.abs_diff(right);
        if diff < best_diff {
            best_diff = diff;
            best_idx = mid;
        }
        prefix += footprint;
    }

    best_idx
}

pub(super) fn choose_internal_rebalance_promotion_idx(
    separators: &[OwnedInternalCell],
) -> Result<usize, DbError> {
    debug_assert!(!separators.is_empty());

    let cap = clustered_internal::page_capacity_bytes();
    let footprints: Vec<usize> = separators
        .iter()
        .map(|cell| clustered_internal::separator_footprint(cell.key.len()))
        .collect();
    let total: usize = footprints.iter().sum();
    let mut prefix = 0usize;
    let mut best_idx = None;
    let mut best_diff = usize::MAX;

    for (mid, footprint) in footprints.iter().copied().enumerate() {
        let left = prefix;
        let right = total - prefix - footprint;
        prefix += footprint;

        if left > cap || right > cap {
            continue;
        }

        let diff = left.abs_diff(right);
        if diff < best_diff {
            best_diff = diff;
            best_idx = Some(mid);
        }
    }

    best_idx.ok_or_else(|| DbError::BTreeCorrupted {
        msg: "clustered internal rebalance found no valid promotion boundary".into(),
    })
}

pub(super) fn choose_balanced_boundary(footprints: &[usize]) -> usize {
    debug_assert!(footprints.len() >= 2);

    let total: usize = footprints.iter().sum();
    let mut left = 0usize;
    let mut best_idx = 1usize;
    let mut best_diff = usize::MAX;

    for split_at in 1..footprints.len() {
        left += footprints[split_at - 1];
        let right = total - left;
        let diff = left.abs_diff(right);
        if diff < best_diff {
            best_diff = diff;
            best_idx = split_at;
        }
    }

    best_idx
}

pub(super) fn materialize_leaf_cell(
    storage: &mut dyn StorageEngine,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<OwnedLeafCell, DbError> {
    validate_row_payload(key, row_data)?;

    let total_row_len = row_data.len();
    let local_len = clustered_leaf::local_row_len(key.len(), total_row_len);
    let local_row_data = row_data[..local_len].to_vec();
    let overflow_first_page = if total_row_len > local_len {
        clustered_overflow::write_chain(storage, &row_data[local_len..])?
    } else {
        None
    };

    Ok(OwnedLeafCell {
        key: key.to_vec(),
        row_header: *row_header,
        total_row_len,
        local_row_data,
        overflow_first_page,
    })
}

pub(super) fn reconstruct_row_data(
    storage: &dyn StorageEngine,
    cell: &clustered_leaf::CellRef<'_>,
) -> Result<Vec<u8>, DbError> {
    let mut row_data = Vec::with_capacity(cell.total_row_len);
    row_data.extend_from_slice(cell.row_data);

    let tail_len = cell.total_row_len.saturating_sub(cell.row_data.len());
    match (tail_len, cell.overflow_first_page) {
        (0, None) => {}
        (tail_len, Some(first_page)) => {
            row_data.extend_from_slice(&clustered_overflow::read_chain(
                storage, first_page, tail_len,
            )?);
        }
        _ => {
            return Err(DbError::BTreeCorrupted {
                msg: format!(
                    "clustered row at key {:?} has inconsistent local/overflow layout",
                    cell.key
                ),
            });
        }
    }

    Ok(row_data)
}

pub(super) fn validate_row_payload(key: &[u8], row_data: &[u8]) -> Result<(), DbError> {
    if row_data.len() > u32::MAX as usize {
        return Err(DbError::ValueTooLarge {
            len: row_data.len(),
            max: u32::MAX as usize,
        });
    }

    let needs_overflow = clustered_leaf::requires_overflow(key.len(), row_data.len());
    let max_key_len = if needs_overflow {
        clustered_leaf::max_overflow_key_bytes()
    } else {
        clustered_leaf::max_inline_key_bytes()
    };
    if key.len() > max_key_len {
        return Err(DbError::KeyTooLong {
            len: key.len(),
            max: max_key_len,
        });
    }

    if !clustered_leaf::fits_inline(key.len(), row_data.len()) {
        return Err(DbError::KeyTooLong {
            len: key.len(),
            max: clustered_leaf::max_inline_key_bytes(),
        });
    }

    Ok(())
}

pub(super) fn bounds_empty(from: &Bound<Vec<u8>>, to: &Bound<Vec<u8>>) -> bool {
    let (lower, upper) = match (from, to) {
        (Bound::Included(lo), Bound::Included(hi))
        | (Bound::Included(lo), Bound::Excluded(hi))
        | (Bound::Excluded(lo), Bound::Included(hi))
        | (Bound::Excluded(lo), Bound::Excluded(hi)) => (lo, hi),
        _ => return false,
    };

    match lower.cmp(upper) {
        std::cmp::Ordering::Greater => true,
        std::cmp::Ordering::Equal => {
            !matches!(from, Bound::Included(_)) || !matches!(to, Bound::Included(_))
        }
        std::cmp::Ordering::Less => false,
    }
}

pub(super) fn clustered_page_type(page: &Page) -> Result<PageType, DbError> {
    let pid = page.header().page_id;
    let page_type =
        PageType::try_from(page.header().page_type).map_err(|err| DbError::BTreeCorrupted {
            msg: format!("clustered tree page {pid} has invalid page type byte: {err}"),
        })?;

    match page_type {
        PageType::ClusteredLeaf | PageType::ClusteredInternal => Ok(page_type),
        other => Err(DbError::BTreeCorrupted {
            msg: format!("clustered tree expected clustered page at {pid}, found {other:?}"),
        }),
    }
}

pub(super) fn write_page(
    storage: &mut dyn StorageEngine,
    pid: u64,
    page: &mut Page,
) -> Result<(), DbError> {
    page.update_checksum();
    storage.write_page(pid, page)
}
