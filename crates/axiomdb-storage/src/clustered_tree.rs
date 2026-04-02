//! Clustered B-tree insert path built on top of clustered leaf/internal pages.
//!
//! This module is intentionally storage-first: it provides the first mutable
//! tree controller for clustered pages without yet replacing the heap+index
//! executor path.

use std::ops::Bound;

use axiomdb_core::{error::DbError, TransactionSnapshot};

use crate::{
    clustered_internal, clustered_leaf,
    heap::RowHeader,
    page::{Page, PageType},
    StorageEngine,
};

#[derive(Debug, Clone)]
struct OwnedLeafCell {
    key: Vec<u8>,
    row_header: RowHeader,
    row_data: Vec<u8>,
}

#[derive(Debug, Clone)]
struct OwnedInternalCell {
    key: Vec<u8>,
    right_child: u64,
}

/// Prefetch depth for sequential clustered leaf scans.
///
/// MariaDB and PostgreSQL both keep a bounded read-ahead window on sequential
/// scans instead of prefetching the whole relation. We use a conservative depth
/// of 4 pages for the clustered leaf chain.
const PREFETCH_DEPTH: u64 = 4;

enum InsertResult {
    Inserted,
    Split { sep_key: Vec<u8>, right_pid: u64 },
}

#[derive(Debug, Clone)]
pub struct ClusteredRow {
    pub key: Vec<u8>,
    pub row_header: RowHeader,
    pub row_data: Vec<u8>,
}

/// Lazy iterator over a primary-key range stored directly in clustered leaves.
pub struct ClusteredRangeIter<'a> {
    storage: &'a dyn StorageEngine,
    current_pid: u64,
    next_leaf_cache: u64,
    slot_idx: usize,
    from: Bound<Vec<u8>>,
    to: Bound<Vec<u8>>,
    snapshot: TransactionSnapshot,
    done: bool,
}

/// Inserts one full row into a clustered B-tree and returns the effective root
/// page id after the operation.
pub fn insert(
    storage: &mut dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<u64, DbError> {
    validate_inline_row(key, row_data)?;

    match root_pid {
        Some(pid) => match insert_subtree(storage, pid, key, row_header, row_data)? {
            InsertResult::Inserted => Ok(pid),
            InsertResult::Split { sep_key, right_pid } => {
                let new_root_pid = storage.alloc_page(PageType::ClusteredInternal)?;
                let mut new_root = Page::new(PageType::ClusteredInternal, new_root_pid);
                clustered_internal::init_clustered_internal(&mut new_root, pid);
                clustered_internal::insert_at(&mut new_root, 0, &sep_key, right_pid)?;
                write_page(storage, new_root_pid, &mut new_root)?;
                Ok(new_root_pid)
            }
        },
        None => {
            let root_pid = storage.alloc_page(PageType::ClusteredLeaf)?;
            let mut root = Page::new(PageType::ClusteredLeaf, root_pid);
            clustered_leaf::init_clustered_leaf(&mut root);
            clustered_leaf::insert_cell(&mut root, 0, key, row_header, row_data)?;
            write_page(storage, root_pid, &mut root)?;
            Ok(root_pid)
        }
    }
}

/// Looks up one full row by primary key in a clustered B-tree and returns the
/// current inline version when it is visible to `snapshot`.
pub fn lookup(
    storage: &dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    snapshot: &TransactionSnapshot,
) -> Result<Option<ClusteredRow>, DbError> {
    let Some(root_pid) = root_pid else {
        return Ok(None);
    };

    let leaf = descend_to_leaf(storage, root_pid, key)?;
    let pos = match leaf_search_checked(&leaf, key)? {
        Ok(pos) => pos,
        Err(_) => return Ok(None),
    };

    let cell = clustered_leaf::read_cell(&leaf, pos as u16)?;
    if !cell.row_header.is_visible(snapshot) {
        return Ok(None);
    }

    Ok(Some(ClusteredRow {
        key: cell.key.to_vec(),
        row_header: cell.row_header,
        row_data: cell.row_data.to_vec(),
    }))
}

/// Builds a lazy range iterator over clustered leaf pages.
pub fn range<'a>(
    storage: &'a dyn StorageEngine,
    root_pid: Option<u64>,
    from: Bound<Vec<u8>>,
    to: Bound<Vec<u8>>,
    snapshot: &TransactionSnapshot,
) -> Result<ClusteredRangeIter<'a>, DbError> {
    if bounds_empty(&from, &to) {
        return Ok(ClusteredRangeIter::empty(storage, from, to, *snapshot));
    }

    let Some(root_pid) = root_pid else {
        return Ok(ClusteredRangeIter::empty(storage, from, to, *snapshot));
    };

    let (current_pid, slot_idx) = find_start_position(storage, root_pid, &from)?;

    Ok(ClusteredRangeIter {
        storage,
        current_pid,
        next_leaf_cache: clustered_leaf::NULL_PAGE,
        slot_idx,
        from,
        to,
        snapshot: *snapshot,
        done: false,
    })
}

/// Rewrites the current inline version of one clustered row in the owning leaf
/// page without changing the primary key or tree structure.
pub fn update_in_place(
    storage: &mut dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    new_row_data: &[u8],
    txn_id: u64,
    snapshot: &TransactionSnapshot,
) -> Result<bool, DbError> {
    validate_inline_row(key, new_row_data)?;

    let Some(root_pid) = root_pid else {
        return Ok(false);
    };

    let leaf_ref = descend_to_leaf(storage, root_pid, key)?;
    let pos = match leaf_search_checked(&leaf_ref, key)? {
        Ok(pos) => pos,
        Err(_) => return Ok(false),
    };

    let (old_header, old_row_len) = {
        let cell = clustered_leaf::read_cell(&leaf_ref, pos as u16)?;
        if !cell.row_header.is_visible(snapshot) {
            return Ok(false);
        }
        (cell.row_header, cell.row_data.len())
    };

    let mut page = leaf_ref.into_page();
    let page_id = page.header().page_id;
    let available =
        clustered_leaf::free_space(&page) + clustered_leaf::cell_footprint(key.len(), old_row_len);
    let needed = clustered_leaf::cell_footprint(key.len(), new_row_data.len());
    let new_header = RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version: old_header.row_version.saturating_add(1),
        _flags: old_header._flags,
    };

    match clustered_leaf::rewrite_cell_same_key(&mut page, pos, key, &new_header, new_row_data)? {
        Some(_) => {
            write_page(storage, page_id, &mut page)?;
            Ok(true)
        }
        None => Err(DbError::HeapPageFull {
            page_id,
            needed,
            available,
        }),
    }
}

/// Applies an MVCC delete-mark to the current inline version of one clustered
/// row without removing the physical cell from the owning leaf page.
pub fn delete_mark(
    storage: &mut dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    txn_id: u64,
    snapshot: &TransactionSnapshot,
) -> Result<bool, DbError> {
    let Some(root_pid) = root_pid else {
        return Ok(false);
    };

    let leaf_ref = descend_to_leaf(storage, root_pid, key)?;
    let pos = match leaf_search_checked(&leaf_ref, key)? {
        Ok(pos) => pos,
        Err(_) => return Ok(false),
    };

    let (old_header, old_row_data) = {
        let cell = clustered_leaf::read_cell(&leaf_ref, pos as u16)?;
        if !cell.row_header.is_visible(snapshot) {
            return Ok(false);
        }
        (cell.row_header, cell.row_data.to_vec())
    };

    let mut page = leaf_ref.into_page();
    let page_id = page.header().page_id;
    let new_header = RowHeader {
        txn_id_created: old_header.txn_id_created,
        txn_id_deleted: txn_id,
        row_version: old_header.row_version,
        _flags: old_header._flags,
    };

    match clustered_leaf::rewrite_cell_same_key(&mut page, pos, key, &new_header, &old_row_data)? {
        Some(_) => {
            write_page(storage, page_id, &mut page)?;
            Ok(true)
        }
        None => Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered delete-mark unexpectedly required a page rebuild failure at page {page_id}"
            ),
        }),
    }
}

fn insert_subtree(
    storage: &mut dyn StorageEngine,
    pid: u64,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<InsertResult, DbError> {
    let page_ref = storage.read_page(pid)?;
    match clustered_page_type(&page_ref)? {
        PageType::ClusteredLeaf => insert_into_leaf(
            storage,
            pid,
            page_ref.into_page(),
            key,
            row_header,
            row_data,
        ),
        PageType::ClusteredInternal => insert_into_internal(
            storage,
            pid,
            page_ref.into_page(),
            key,
            row_header,
            row_data,
        ),
        other => Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered tree encountered unsupported page type {other:?} at page {pid}"
            ),
        }),
    }
}

fn insert_into_leaf(
    storage: &mut dyn StorageEngine,
    pid: u64,
    mut page: Page,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<InsertResult, DbError> {
    let insert_pos = match leaf_search_checked(&page, key)? {
        Ok(_) => return Err(DbError::DuplicateKey),
        Err(pos) => pos,
    };

    match clustered_leaf::insert_cell(&mut page, insert_pos, key, row_header, row_data) {
        Ok(()) => {
            write_page(storage, pid, &mut page)?;
            Ok(InsertResult::Inserted)
        }
        Err(DbError::HeapPageFull { .. }) => {
            clustered_leaf::defragment(&mut page);
            match clustered_leaf::insert_cell(&mut page, insert_pos, key, row_header, row_data) {
                Ok(()) => {
                    write_page(storage, pid, &mut page)?;
                    Ok(InsertResult::Inserted)
                }
                Err(DbError::HeapPageFull { .. }) => {
                    split_leaf(storage, pid, &page, insert_pos, key, row_header, row_data)
                }
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    }
}

fn split_leaf(
    storage: &mut dyn StorageEngine,
    pid: u64,
    page: &Page,
    insert_pos: usize,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<InsertResult, DbError> {
    let mut cells = collect_leaf_cells(page)?;
    cells.insert(
        insert_pos,
        OwnedLeafCell {
            key: key.to_vec(),
            row_header: *row_header,
            row_data: row_data.to_vec(),
        },
    );

    let split_at = choose_leaf_split_idx(&cells);
    let old_next_leaf = clustered_leaf::next_leaf(page);
    let right_pid = storage.alloc_page(PageType::ClusteredLeaf)?;

    let mut left_page = Page::new(PageType::ClusteredLeaf, pid);
    rebuild_leaf_page(&mut left_page, &cells[..split_at], right_pid)?;
    let mut right_page = Page::new(PageType::ClusteredLeaf, right_pid);
    rebuild_leaf_page(&mut right_page, &cells[split_at..], old_next_leaf)?;

    write_page(storage, pid, &mut left_page)?;
    write_page(storage, right_pid, &mut right_page)?;

    Ok(InsertResult::Split {
        sep_key: cells[split_at].key.clone(),
        right_pid,
    })
}

fn insert_into_internal(
    storage: &mut dyn StorageEngine,
    pid: u64,
    mut page: Page,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<InsertResult, DbError> {
    let child_idx = clustered_internal::find_child_idx(&page, key)?;
    let child_pid = clustered_internal::child_at(&page, child_idx as u16)?;

    match insert_subtree(storage, child_pid, key, row_header, row_data)? {
        InsertResult::Inserted => Ok(InsertResult::Inserted),
        InsertResult::Split { sep_key, right_pid } => {
            match clustered_internal::insert_at(&mut page, child_idx, &sep_key, right_pid) {
                Ok(()) => {
                    write_page(storage, pid, &mut page)?;
                    Ok(InsertResult::Inserted)
                }
                Err(DbError::HeapPageFull { .. }) => {
                    clustered_internal::defragment(&mut page);
                    match clustered_internal::insert_at(&mut page, child_idx, &sep_key, right_pid) {
                        Ok(()) => {
                            write_page(storage, pid, &mut page)?;
                            Ok(InsertResult::Inserted)
                        }
                        Err(DbError::HeapPageFull { .. }) => {
                            split_internal(storage, pid, &page, child_idx, &sep_key, right_pid)
                        }
                        Err(err) => Err(err),
                    }
                }
                Err(err) => Err(err),
            }
        }
    }
}

fn split_internal(
    storage: &mut dyn StorageEngine,
    pid: u64,
    page: &Page,
    insert_pos: usize,
    sep_key: &[u8],
    right_pid: u64,
) -> Result<InsertResult, DbError> {
    let leftmost_child = clustered_internal::leftmost_child(page);
    let mut separators = collect_internal_cells(page)?;
    separators.insert(
        insert_pos,
        OwnedInternalCell {
            key: sep_key.to_vec(),
            right_child: right_pid,
        },
    );

    let promoted_idx = choose_internal_promotion_idx(&separators);
    let promoted = separators[promoted_idx].clone();
    let new_right_pid = storage.alloc_page(PageType::ClusteredInternal)?;

    let mut left_page = Page::new(PageType::ClusteredInternal, pid);
    rebuild_internal_page(&mut left_page, leftmost_child, &separators[..promoted_idx])?;

    let mut right_page = Page::new(PageType::ClusteredInternal, new_right_pid);
    rebuild_internal_page(
        &mut right_page,
        promoted.right_child,
        &separators[promoted_idx + 1..],
    )?;

    write_page(storage, pid, &mut left_page)?;
    write_page(storage, new_right_pid, &mut right_page)?;

    Ok(InsertResult::Split {
        sep_key: promoted.key,
        right_pid: new_right_pid,
    })
}

fn collect_leaf_cells(page: &Page) -> Result<Vec<OwnedLeafCell>, DbError> {
    let num_cells = clustered_leaf::num_cells(page);
    let mut cells = Vec::with_capacity(num_cells as usize);
    for idx in 0..num_cells {
        let cell = clustered_leaf::read_cell(page, idx)?;
        cells.push(OwnedLeafCell {
            key: cell.key.to_vec(),
            row_header: cell.row_header,
            row_data: cell.row_data.to_vec(),
        });
    }
    Ok(cells)
}

fn descend_to_leaf(
    storage: &dyn StorageEngine,
    mut pid: u64,
    key: &[u8],
) -> Result<crate::PageRef, DbError> {
    loop {
        let page = storage.read_page(pid)?;
        match clustered_page_type(&page)? {
            PageType::ClusteredLeaf => return Ok(page),
            PageType::ClusteredInternal => {
                let child_idx = clustered_internal::find_child_idx(&page, key)?;
                pid = clustered_internal::child_at(&page, child_idx as u16)?;
            }
            other => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!(
                        "clustered tree encountered unsupported page type {other:?} at page {pid}"
                    ),
                });
            }
        }
    }
}

fn leftmost_leaf_pid(storage: &dyn StorageEngine, mut pid: u64) -> Result<u64, DbError> {
    loop {
        let page = storage.read_page(pid)?;
        match clustered_page_type(&page)? {
            PageType::ClusteredLeaf => return Ok(pid),
            PageType::ClusteredInternal => {
                pid = clustered_internal::child_at(&page, 0)?;
            }
            other => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!(
                        "clustered tree encountered unsupported page type {other:?} at page {pid}"
                    ),
                });
            }
        }
    }
}

fn find_start_position(
    storage: &dyn StorageEngine,
    root_pid: u64,
    from: &Bound<Vec<u8>>,
) -> Result<(u64, usize), DbError> {
    match from {
        Bound::Unbounded => Ok((leftmost_leaf_pid(storage, root_pid)?, 0)),
        Bound::Included(key) => {
            let leaf = descend_to_leaf(storage, root_pid, key)?;
            let slot_idx = match leaf_search_checked(&leaf, key)? {
                Ok(pos) | Err(pos) => pos,
            };
            Ok((leaf.header().page_id, slot_idx))
        }
        Bound::Excluded(key) => {
            let leaf = descend_to_leaf(storage, root_pid, key)?;
            let slot_idx = match leaf_search_checked(&leaf, key)? {
                Ok(pos) => pos + 1,
                Err(pos) => pos,
            };
            Ok((leaf.header().page_id, slot_idx))
        }
    }
}

fn collect_internal_cells(page: &Page) -> Result<Vec<OwnedInternalCell>, DbError> {
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

fn rebuild_leaf_page(
    page: &mut Page,
    cells: &[OwnedLeafCell],
    next_leaf_pid: u64,
) -> Result<(), DbError> {
    *page = Page::new(PageType::ClusteredLeaf, page.header().page_id);
    clustered_leaf::init_clustered_leaf(page);
    clustered_leaf::set_next_leaf(page, next_leaf_pid);
    for (idx, cell) in cells.iter().enumerate() {
        clustered_leaf::insert_cell(page, idx, &cell.key, &cell.row_header, &cell.row_data)?;
    }
    Ok(())
}

fn rebuild_internal_page(
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

fn leaf_search_checked(page: &Page, key: &[u8]) -> Result<Result<usize, usize>, DbError> {
    let n = clustered_leaf::num_cells(page) as usize;
    let mut lo = 0usize;
    let mut hi = n;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let cell = clustered_leaf::read_cell(page, mid as u16)?;
        match cell.key.cmp(key) {
            std::cmp::Ordering::Equal => return Ok(Ok(mid)),
            std::cmp::Ordering::Less => lo = mid + 1,
            std::cmp::Ordering::Greater => hi = mid,
        }
    }
    Ok(Err(lo))
}

fn choose_leaf_split_idx(cells: &[OwnedLeafCell]) -> usize {
    debug_assert!(cells.len() >= 2);
    let footprints: Vec<usize> = cells
        .iter()
        .map(|cell| clustered_leaf::cell_footprint(cell.key.len(), cell.row_data.len()))
        .collect();
    choose_balanced_boundary(&footprints)
}

fn choose_internal_promotion_idx(separators: &[OwnedInternalCell]) -> usize {
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

fn choose_balanced_boundary(footprints: &[usize]) -> usize {
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

fn validate_inline_row(key: &[u8], row_data: &[u8]) -> Result<(), DbError> {
    if key.len() > clustered_leaf::max_inline_key_bytes() {
        return Err(DbError::KeyTooLong {
            len: key.len(),
            max: clustered_leaf::max_inline_key_bytes(),
        });
    }

    let Some(max_row_len) = clustered_leaf::max_inline_row_bytes(key.len()) else {
        return Err(DbError::KeyTooLong {
            len: key.len(),
            max: clustered_leaf::max_inline_key_bytes(),
        });
    };

    if row_data.len() > max_row_len {
        return Err(DbError::ValueTooLarge {
            len: row_data.len(),
            max: max_row_len,
        });
    }

    Ok(())
}

fn bounds_empty(from: &Bound<Vec<u8>>, to: &Bound<Vec<u8>>) -> bool {
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

fn clustered_page_type(page: &Page) -> Result<PageType, DbError> {
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

fn write_page(storage: &mut dyn StorageEngine, pid: u64, page: &mut Page) -> Result<(), DbError> {
    page.update_checksum();
    storage.write_page(pid, page)
}

impl<'a> ClusteredRangeIter<'a> {
    fn empty(
        storage: &'a dyn StorageEngine,
        from: Bound<Vec<u8>>,
        to: Bound<Vec<u8>>,
        snapshot: TransactionSnapshot,
    ) -> Self {
        Self {
            storage,
            current_pid: clustered_leaf::NULL_PAGE,
            next_leaf_cache: clustered_leaf::NULL_PAGE,
            slot_idx: 0,
            from,
            to,
            snapshot,
            done: true,
        }
    }

    fn above_lower(&self, key: &[u8]) -> bool {
        match &self.from {
            Bound::Unbounded => true,
            Bound::Included(lo) => key >= lo.as_slice(),
            Bound::Excluded(lo) => key > lo.as_slice(),
        }
    }

    fn below_upper(&self, key: &[u8]) -> bool {
        match &self.to {
            Bound::Unbounded => true,
            Bound::Included(hi) => key <= hi.as_slice(),
            Bound::Excluded(hi) => key < hi.as_slice(),
        }
    }
}

impl Iterator for ClusteredRangeIter<'_> {
    type Item = Result<ClusteredRow, DbError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }

        loop {
            if self.current_pid == clustered_leaf::NULL_PAGE {
                self.done = true;
                return None;
            }

            let page = match self.storage.read_page(self.current_pid) {
                Ok(page) => page,
                Err(err) => return Some(Err(err)),
            };

            match clustered_page_type(&page) {
                Ok(PageType::ClusteredLeaf) => {}
                Ok(other) => {
                    return Some(Err(DbError::BTreeCorrupted {
                        msg: format!(
                            "clustered range scan expected leaf at page {}, found {other:?}",
                            self.current_pid
                        ),
                    }));
                }
                Err(err) => return Some(Err(err)),
            }

            if self.next_leaf_cache == clustered_leaf::NULL_PAGE {
                self.next_leaf_cache = clustered_leaf::next_leaf(&page);
            }

            let num_cells = clustered_leaf::num_cells(&page) as usize;
            while self.slot_idx < num_cells {
                let idx = self.slot_idx as u16;
                self.slot_idx += 1;

                let cell = match clustered_leaf::read_cell(&page, idx) {
                    Ok(cell) => cell,
                    Err(err) => return Some(Err(err)),
                };

                if !self.above_lower(cell.key) {
                    continue;
                }
                if !self.below_upper(cell.key) {
                    self.done = true;
                    return None;
                }
                if !cell.row_header.is_visible(&self.snapshot) {
                    continue;
                }

                return Some(Ok(ClusteredRow {
                    key: cell.key.to_vec(),
                    row_header: cell.row_header,
                    row_data: cell.row_data.to_vec(),
                }));
            }

            let next_pid = self.next_leaf_cache;
            if next_pid == clustered_leaf::NULL_PAGE {
                self.done = true;
                return None;
            }

            self.storage.prefetch_hint(next_pid, PREFETCH_DEPTH);
            self.current_pid = next_pid;
            self.next_leaf_cache = clustered_leaf::NULL_PAGE;
            self.slot_idx = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    use crate::{clustered_internal, clustered_leaf, MemoryStorage, PageRef};
    use axiomdb_core::TransactionSnapshot;

    fn row_header(txn_id: u64) -> RowHeader {
        RowHeader {
            txn_id_created: txn_id,
            txn_id_deleted: 0,
            row_version: 0,
            _flags: 0,
        }
    }

    fn row_bytes(seed: u32, len: usize) -> Vec<u8> {
        (0..len)
            .map(|idx| ((seed as usize + idx) % 251) as u8)
            .collect()
    }

    fn committed_snapshot(max_committed: u64) -> TransactionSnapshot {
        TransactionSnapshot::committed(max_committed)
    }

    fn active_snapshot(txn_id: u64, max_committed: u64) -> TransactionSnapshot {
        TransactionSnapshot::active(txn_id, max_committed)
    }

    fn leftmost_leaf_pid(storage: &dyn StorageEngine, mut pid: u64) -> Result<u64, DbError> {
        loop {
            let page = storage.read_page(pid)?;
            match clustered_page_type(&page)? {
                PageType::ClusteredLeaf => return Ok(pid),
                PageType::ClusteredInternal => {
                    pid = clustered_internal::child_at(&page, 0)?;
                }
                other => {
                    return Err(DbError::BTreeCorrupted {
                        msg: format!("unexpected page type in leftmost_leaf_pid: {other:?}"),
                    });
                }
            }
        }
    }

    fn collect_leaf_chain_keys(
        storage: &dyn StorageEngine,
        root_pid: u64,
    ) -> Result<Vec<Vec<u8>>, DbError> {
        let mut leaf_pid = leftmost_leaf_pid(storage, root_pid)?;
        let mut keys = Vec::new();

        loop {
            let page = storage.read_page(leaf_pid)?;
            assert_eq!(
                clustered_page_type(&page)?,
                PageType::ClusteredLeaf,
                "leaf chain must contain only clustered leaves"
            );

            for idx in 0..clustered_leaf::num_cells(&page) {
                keys.push(clustered_leaf::read_cell(&page, idx)?.key.to_vec());
            }

            let next = clustered_leaf::next_leaf(&page);
            if next == clustered_leaf::NULL_PAGE {
                break;
            }
            leaf_pid = next;
        }

        Ok(keys)
    }

    fn collect_range_rows(iter: ClusteredRangeIter<'_>) -> Result<Vec<ClusteredRow>, DbError> {
        iter.collect()
    }

    struct CountingPrefetchStorage {
        inner: MemoryStorage,
        prefetches: Arc<AtomicUsize>,
    }

    impl StorageEngine for CountingPrefetchStorage {
        fn read_page(&self, page_id: u64) -> Result<PageRef, DbError> {
            self.inner.read_page(page_id)
        }

        fn write_page(&mut self, page_id: u64, page: &Page) -> Result<(), DbError> {
            self.inner.write_page(page_id, page)
        }

        fn alloc_page(&mut self, page_type: PageType) -> Result<u64, DbError> {
            self.inner.alloc_page(page_type)
        }

        fn free_page(&mut self, page_id: u64) -> Result<(), DbError> {
            self.inner.free_page(page_id)
        }

        fn flush(&mut self) -> Result<(), DbError> {
            self.inner.flush()
        }

        fn page_count(&self) -> u64 {
            self.inner.page_count()
        }

        fn prefetch_hint(&self, start_page_id: u64, count: u64) {
            self.prefetches.fetch_add(1, Ordering::Relaxed);
            self.inner.prefetch_hint(start_page_id, count);
        }
    }

    #[test]
    fn insert_bootstraps_empty_tree() {
        let mut storage = MemoryStorage::new();
        let root = insert(&mut storage, None, b"pk-1", &row_header(11), b"row-1").unwrap();
        let page = storage.read_page(root).unwrap();

        assert_eq!(clustered_page_type(&page).unwrap(), PageType::ClusteredLeaf);
        assert_eq!(clustered_leaf::num_cells(&page), 1);
        let cell = clustered_leaf::read_cell(&page, 0).unwrap();
        assert_eq!(cell.key, b"pk-1");
        assert_eq!(cell.row_data, b"row-1");
        assert_eq!(cell.row_header.txn_id_created, 11);
    }

    #[test]
    fn duplicate_key_is_rejected() {
        let mut storage = MemoryStorage::new();
        let root = insert(&mut storage, None, b"dup", &row_header(1), b"a").unwrap();
        let err = insert(&mut storage, Some(root), b"dup", &row_header(2), b"b").unwrap_err();
        assert!(matches!(err, DbError::DuplicateKey));
    }

    #[test]
    fn non_split_leaf_insert_preserves_sorted_order() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in [b"charlie".as_slice(), b"alpha", b"bravo"] {
            root = Some(insert(&mut storage, root, key, &row_header(1), b"row").unwrap());
        }

        let keys = collect_leaf_chain_keys(&storage, root.unwrap()).unwrap();
        assert_eq!(
            keys,
            vec![b"alpha".to_vec(), b"bravo".to_vec(), b"charlie".to_vec()]
        );
    }

    #[test]
    fn defrag_happens_before_split() {
        let mut storage = MemoryStorage::new();
        let root_pid = storage.alloc_page(PageType::ClusteredLeaf).unwrap();
        let mut root = Page::new(PageType::ClusteredLeaf, root_pid);
        clustered_leaf::init_clustered_leaf(&mut root);

        let hdr = row_header(1);
        let filler = vec![7u8; 2_000];
        for key in 1u32..=7 {
            let pos = clustered_leaf::num_cells(&root) as usize;
            clustered_leaf::insert_cell(&mut root, pos, &key.to_be_bytes(), &hdr, &filler).unwrap();
        }
        clustered_leaf::remove_cell(&mut root, 3).unwrap();
        clustered_leaf::remove_cell(&mut root, 1).unwrap();
        root.update_checksum();
        storage.write_page(root_pid, &root).unwrap();

        let gap_before = {
            let page = storage.read_page(root_pid).unwrap();
            let free = clustered_leaf::free_space(&page);
            let mut page = page.into_page();
            match clustered_leaf::insert_cell(
                &mut page,
                1,
                &4u32.to_be_bytes(),
                &hdr,
                &vec![9u8; 3_000],
            ) {
                Ok(()) => panic!("test setup should require defragmentation"),
                Err(DbError::HeapPageFull { .. }) => {}
                Err(err) => panic!("unexpected setup error: {err}"),
            }
            free
        };
        assert!(gap_before >= clustered_leaf::cell_footprint(4, 3_000));

        let root_after = insert(
            &mut storage,
            Some(root_pid),
            &4u32.to_be_bytes(),
            &hdr,
            &vec![9u8; 3_000],
        )
        .unwrap();
        assert_eq!(root_after, root_pid, "defrag should avoid a split");

        let page = storage.read_page(root_pid).unwrap();
        assert_eq!(clustered_page_type(&page).unwrap(), PageType::ClusteredLeaf);
        assert_eq!(clustered_leaf::num_cells(&page), 6);
        let keys: Vec<Vec<u8>> = (0..clustered_leaf::num_cells(&page))
            .map(|idx| clustered_leaf::read_cell(&page, idx).unwrap().key.to_vec())
            .collect();
        assert_eq!(
            keys,
            vec![
                1u32.to_be_bytes().to_vec(),
                3u32.to_be_bytes().to_vec(),
                4u32.to_be_bytes().to_vec(),
                5u32.to_be_bytes().to_vec(),
                6u32.to_be_bytes().to_vec(),
                7u32.to_be_bytes().to_vec(),
            ]
        );
    }

    #[test]
    fn leaf_split_sets_separator_and_next_leaf_chain() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..8 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &vec![key as u8; 2_700],
                )
                .unwrap(),
            );
        }

        let root_pid = root.unwrap();
        let root_page = storage.read_page(root_pid).unwrap();
        assert_eq!(
            clustered_page_type(&root_page).unwrap(),
            PageType::ClusteredInternal
        );
        assert_eq!(clustered_internal::num_cells(&root_page), 1);

        let left_pid = clustered_internal::child_at(&root_page, 0).unwrap();
        let right_pid = clustered_internal::child_at(&root_page, 1).unwrap();
        let left = storage.read_page(left_pid).unwrap();
        let right = storage.read_page(right_pid).unwrap();

        assert_eq!(clustered_leaf::next_leaf(&left), right_pid);
        assert_eq!(clustered_leaf::next_leaf(&right), clustered_leaf::NULL_PAGE);
        let sep = clustered_internal::key_at(&root_page, 0).unwrap().to_vec();
        let right_first = clustered_leaf::read_cell(&right, 0).unwrap().key.to_vec();
        assert_eq!(sep, right_first);

        let keys = collect_leaf_chain_keys(&storage, root_pid).unwrap();
        let expected: Vec<Vec<u8>> = (0u32..8).map(|v| v.to_be_bytes().to_vec()).collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn internal_split_and_root_split_keep_keys_reachable() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..64 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &vec![key as u8; 3_200],
                )
                .unwrap(),
            );
        }

        let root_pid = root.unwrap();
        let root_page = storage.read_page(root_pid).unwrap();
        assert_eq!(
            clustered_page_type(&root_page).unwrap(),
            PageType::ClusteredInternal
        );
        assert!(
            clustered_internal::num_cells(&root_page) >= 2,
            "expected root to absorb multiple separators after deeper splits"
        );

        let keys = collect_leaf_chain_keys(&storage, root_pid).unwrap();
        let expected: Vec<Vec<u8>> = (0u32..64).map(|v| v.to_be_bytes().to_vec()).collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn rows_that_need_overflow_are_rejected() {
        let mut storage = MemoryStorage::new();
        let key = b"overflow-pk";
        let max = clustered_leaf::max_inline_row_bytes(key.len()).unwrap();
        let err = insert(&mut storage, None, key, &row_header(1), &vec![0u8; max + 1]).unwrap_err();
        assert!(matches!(err, DbError::ValueTooLarge { .. }));
    }

    #[test]
    fn lookup_empty_tree_returns_none() {
        let storage = MemoryStorage::new();
        let got = lookup(&storage, None, b"missing", &committed_snapshot(100)).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn lookup_root_leaf_hit_returns_inline_row() {
        let mut storage = MemoryStorage::new();
        let root = insert(&mut storage, None, b"pk-1", &row_header(3), b"row-inline").unwrap();

        let got = lookup(&storage, Some(root), b"pk-1", &committed_snapshot(10))
            .unwrap()
            .expect("row must exist");

        assert_eq!(got.key, b"pk-1");
        assert_eq!(got.row_data, b"row-inline");
        assert_eq!(got.row_header.txn_id_created, 3);
    }

    #[test]
    fn lookup_missing_key_returns_none() {
        let mut storage = MemoryStorage::new();
        let mut root = None;
        for key in [b"alpha".as_slice(), b"bravo", b"charlie"] {
            root = Some(insert(&mut storage, root, key, &row_header(1), b"row").unwrap());
        }

        let got = lookup(&storage, root, b"delta", &committed_snapshot(10)).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn lookup_invisible_current_version_returns_none() {
        let mut storage = MemoryStorage::new();
        let root = insert(
            &mut storage,
            None,
            b"future",
            &row_header(9),
            b"not-committed-yet",
        )
        .unwrap();

        let invisible = lookup(&storage, Some(root), b"future", &committed_snapshot(4)).unwrap();
        assert!(invisible.is_none());

        let visible_to_self = lookup(&storage, Some(root), b"future", &active_snapshot(9, 4))
            .unwrap()
            .expect("own write must be visible");
        assert_eq!(visible_to_self.row_data, b"not-committed-yet");
    }

    #[test]
    fn lookup_after_internal_splits_finds_exact_row() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..128 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &row_bytes(key, 3_000 + (key as usize % 3) * 97),
                )
                .unwrap(),
            );
        }

        let root = root.unwrap();
        let got = lookup(
            &storage,
            Some(root),
            &93u32.to_be_bytes(),
            &committed_snapshot(10),
        )
        .unwrap()
        .expect("row must exist after splits");

        assert_eq!(got.key, 93u32.to_be_bytes());
        assert_eq!(got.row_data, row_bytes(93, 3_000));
    }

    #[test]
    fn update_in_place_empty_tree_returns_false() {
        let mut storage = MemoryStorage::new();
        let changed = update_in_place(
            &mut storage,
            None,
            b"missing",
            b"new-row",
            9,
            &committed_snapshot(4),
        )
        .unwrap();
        assert!(!changed);
    }

    #[test]
    fn update_in_place_missing_key_returns_false() {
        let mut storage = MemoryStorage::new();
        let mut root = None;
        for key in [b"alpha".as_slice(), b"bravo", b"charlie"] {
            root = Some(insert(&mut storage, root, key, &row_header(1), b"row").unwrap());
        }

        let changed = update_in_place(
            &mut storage,
            root,
            b"delta",
            b"updated",
            9,
            &committed_snapshot(4),
        )
        .unwrap();
        assert!(!changed);
    }

    #[test]
    fn update_in_place_invisible_current_version_returns_false() {
        let mut storage = MemoryStorage::new();
        let root = insert(
            &mut storage,
            None,
            b"future",
            &row_header(9),
            b"not-committed-yet",
        )
        .unwrap();

        let changed = update_in_place(
            &mut storage,
            Some(root),
            b"future",
            b"replacement",
            12,
            &committed_snapshot(4),
        )
        .unwrap();
        assert!(!changed);

        let still_old = lookup(&storage, Some(root), b"future", &active_snapshot(9, 4))
            .unwrap()
            .expect("original row must stay unchanged");
        assert_eq!(still_old.row_data, b"not-committed-yet");
        assert_eq!(still_old.row_header.txn_id_created, 9);
    }

    #[test]
    fn update_in_place_root_leaf_growth_rewrites_row_and_bumps_version() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..4 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &vec![key as u8; 400],
                )
                .unwrap(),
            );
        }

        let root = root.unwrap();
        let changed = update_in_place(
            &mut storage,
            Some(root),
            &2u32.to_be_bytes(),
            &vec![7u8; 2_000],
            9,
            &active_snapshot(9, 1),
        )
        .unwrap();
        assert!(changed);

        let row = lookup(
            &storage,
            Some(root),
            &2u32.to_be_bytes(),
            &active_snapshot(9, 1),
        )
        .unwrap()
        .expect("updated row must be visible to updater");
        assert_eq!(row.key, 2u32.to_be_bytes());
        assert_eq!(row.row_data, vec![7u8; 2_000]);
        assert_eq!(row.row_header.txn_id_created, 9);
        assert_eq!(row.row_header.row_version, 1);

        let old_snapshot = lookup(
            &storage,
            Some(root),
            &2u32.to_be_bytes(),
            &committed_snapshot(1),
        )
        .unwrap();
        assert!(old_snapshot.is_none());
    }

    #[test]
    fn update_in_place_on_split_tree_preserves_leaf_identity_and_next_link() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..128 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &vec![key as u8; 300],
                )
                .unwrap(),
            );
        }

        let root = root.unwrap();
        let before_leaf = descend_to_leaf(&storage, root, &63u32.to_be_bytes())
            .unwrap()
            .header()
            .page_id;
        let before_next = {
            let page = storage.read_page(before_leaf).unwrap();
            clustered_leaf::next_leaf(&page)
        };

        let changed = update_in_place(
            &mut storage,
            Some(root),
            &63u32.to_be_bytes(),
            &vec![9u8; 700],
            11,
            &active_snapshot(11, 1),
        )
        .unwrap();
        assert!(changed);

        let after_leaf = descend_to_leaf(&storage, root, &63u32.to_be_bytes())
            .unwrap()
            .header()
            .page_id;
        let after_next = {
            let page = storage.read_page(after_leaf).unwrap();
            clustered_leaf::next_leaf(&page)
        };

        assert_eq!(after_leaf, before_leaf);
        assert_eq!(after_next, before_next);

        let row = lookup(
            &storage,
            Some(root),
            &63u32.to_be_bytes(),
            &active_snapshot(11, 1),
        )
        .unwrap()
        .expect("updated row must remain reachable");
        assert_eq!(row.row_data, vec![9u8; 700]);
        assert_eq!(row.row_header.row_version, 1);
    }

    #[test]
    fn update_in_place_returns_heap_page_full_when_growth_cannot_stay_in_leaf() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..7 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &vec![key as u8; 2_100],
                )
                .unwrap(),
            );
        }

        let root = root.unwrap();
        let err = update_in_place(
            &mut storage,
            Some(root),
            &0u32.to_be_bytes(),
            &vec![9u8; 8_000],
            8,
            &active_snapshot(8, 1),
        )
        .unwrap_err();
        assert!(matches!(err, DbError::HeapPageFull { .. }));

        let row = lookup(
            &storage,
            Some(root),
            &0u32.to_be_bytes(),
            &committed_snapshot(1),
        )
        .unwrap()
        .expect("failed update must leave old row intact");
        assert_eq!(row.row_data, vec![0u8; 2_100]);
        assert_eq!(row.row_header.txn_id_created, 1);
    }

    #[test]
    fn delete_mark_empty_tree_returns_false() {
        let mut storage = MemoryStorage::new();
        let changed =
            delete_mark(&mut storage, None, b"missing", 9, &committed_snapshot(4)).unwrap();
        assert!(!changed);
    }

    #[test]
    fn delete_mark_missing_key_returns_false() {
        let mut storage = MemoryStorage::new();
        let mut root = None;
        for key in [b"alpha".as_slice(), b"bravo", b"charlie"] {
            root = Some(insert(&mut storage, root, key, &row_header(1), b"row").unwrap());
        }

        let changed = delete_mark(&mut storage, root, b"delta", 9, &committed_snapshot(4)).unwrap();
        assert!(!changed);
    }

    #[test]
    fn delete_mark_invisible_current_version_returns_false() {
        let mut storage = MemoryStorage::new();
        let root = insert(
            &mut storage,
            None,
            b"future",
            &row_header(9),
            b"not-committed-yet",
        )
        .unwrap();

        let changed = delete_mark(
            &mut storage,
            Some(root),
            b"future",
            12,
            &committed_snapshot(4),
        )
        .unwrap();
        assert!(!changed);

        let still_visible = lookup(&storage, Some(root), b"future", &active_snapshot(9, 4))
            .unwrap()
            .expect("original row must stay unchanged");
        assert_eq!(still_visible.row_data, b"not-committed-yet");
        assert_eq!(still_visible.row_header.txn_id_deleted, 0);
    }

    #[test]
    fn delete_mark_root_leaf_hides_row_from_newer_snapshots_but_preserves_old_visibility() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..4 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &vec![key as u8; 400],
                )
                .unwrap(),
            );
        }

        let root = root.unwrap();
        let deleted = delete_mark(
            &mut storage,
            Some(root),
            &2u32.to_be_bytes(),
            9,
            &active_snapshot(9, 1),
        )
        .unwrap();
        assert!(deleted);

        let hidden_from_deleter = lookup(
            &storage,
            Some(root),
            &2u32.to_be_bytes(),
            &active_snapshot(9, 1),
        )
        .unwrap();
        assert!(hidden_from_deleter.is_none());

        let hidden_from_new_snapshot = lookup(
            &storage,
            Some(root),
            &2u32.to_be_bytes(),
            &committed_snapshot(9),
        )
        .unwrap();
        assert!(hidden_from_new_snapshot.is_none());

        let old_snapshot = lookup(
            &storage,
            Some(root),
            &2u32.to_be_bytes(),
            &committed_snapshot(1),
        )
        .unwrap()
        .expect("older snapshot must still see delete-marked row");
        assert_eq!(old_snapshot.key, 2u32.to_be_bytes());
        assert_eq!(old_snapshot.row_data, vec![2u8; 400]);
        assert_eq!(old_snapshot.row_header.txn_id_created, 1);
        assert_eq!(old_snapshot.row_header.txn_id_deleted, 9);
        assert_eq!(old_snapshot.row_header.row_version, 0);

        let current_rows = collect_range_rows(
            range(
                &storage,
                Some(root),
                Bound::Unbounded,
                Bound::Unbounded,
                &active_snapshot(9, 1),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(current_rows.len(), 3);
        assert!(!current_rows.iter().any(|row| row.key == 2u32.to_be_bytes()));

        let old_rows = collect_range_rows(
            range(
                &storage,
                Some(root),
                Bound::Unbounded,
                Bound::Unbounded,
                &committed_snapshot(1),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(old_rows.len(), 4);
        assert!(old_rows.iter().any(|row| row.key == 2u32.to_be_bytes()));
    }

    #[test]
    fn delete_mark_on_split_tree_preserves_leaf_identity_and_next_link() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..128 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &vec![key as u8; 300],
                )
                .unwrap(),
            );
        }

        let root = root.unwrap();
        let before_leaf = descend_to_leaf(&storage, root, &63u32.to_be_bytes())
            .unwrap()
            .header()
            .page_id;
        let before_next = {
            let page = storage.read_page(before_leaf).unwrap();
            clustered_leaf::next_leaf(&page)
        };

        let deleted = delete_mark(
            &mut storage,
            Some(root),
            &63u32.to_be_bytes(),
            11,
            &active_snapshot(11, 1),
        )
        .unwrap();
        assert!(deleted);

        let after_leaf = descend_to_leaf(&storage, root, &63u32.to_be_bytes())
            .unwrap()
            .header()
            .page_id;
        let after_next = {
            let page = storage.read_page(after_leaf).unwrap();
            clustered_leaf::next_leaf(&page)
        };

        assert_eq!(after_leaf, before_leaf);
        assert_eq!(after_next, before_next);

        let old_snapshot = lookup(
            &storage,
            Some(root),
            &63u32.to_be_bytes(),
            &committed_snapshot(1),
        )
        .unwrap()
        .expect("older snapshot must still see delete-marked row");
        assert_eq!(old_snapshot.row_header.txn_id_deleted, 11);

        let new_snapshot = lookup(
            &storage,
            Some(root),
            &63u32.to_be_bytes(),
            &committed_snapshot(11),
        )
        .unwrap();
        assert!(new_snapshot.is_none());
    }

    #[test]
    fn range_empty_tree_returns_no_rows() {
        let storage = MemoryStorage::new();
        let rows = collect_range_rows(
            range(
                &storage,
                None,
                Bound::Unbounded,
                Bound::Unbounded,
                &committed_snapshot(100),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(rows.is_empty());
    }

    #[test]
    fn range_full_scan_returns_rows_in_primary_key_order() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..128 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header((key % 17) as u64 + 1),
                    &row_bytes(key, 512 + (key as usize % 5) * 71),
                )
                .unwrap(),
            );
        }

        let rows = collect_range_rows(
            range(
                &storage,
                root,
                Bound::Unbounded,
                Bound::Unbounded,
                &committed_snapshot(10_000),
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(rows.len(), 128);
        for (idx, row) in rows.iter().enumerate() {
            assert_eq!(row.key, (idx as u32).to_be_bytes());
        }
    }

    #[test]
    fn range_included_and_excluded_bounds_are_respected() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..32 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &row_bytes(key, 64),
                )
                .unwrap(),
            );
        }

        let inclusive = collect_range_rows(
            range(
                &storage,
                root,
                Bound::Included(10u32.to_be_bytes().to_vec()),
                Bound::Included(15u32.to_be_bytes().to_vec()),
                &committed_snapshot(10),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(inclusive.len(), 6);
        assert_eq!(inclusive.first().unwrap().key, 10u32.to_be_bytes());
        assert_eq!(inclusive.last().unwrap().key, 15u32.to_be_bytes());

        let exclusive = collect_range_rows(
            range(
                &storage,
                root,
                Bound::Excluded(10u32.to_be_bytes().to_vec()),
                Bound::Excluded(15u32.to_be_bytes().to_vec()),
                &committed_snapshot(10),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(exclusive.len(), 4);
        assert_eq!(exclusive.first().unwrap().key, 11u32.to_be_bytes());
        assert_eq!(exclusive.last().unwrap().key, 14u32.to_be_bytes());
    }

    #[test]
    fn range_skips_invisible_current_versions() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..8 {
            let created_by = if key % 2 == 0 { 9 } else { 2 };
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(created_by),
                    &row_bytes(key, 96),
                )
                .unwrap(),
            );
        }

        let rows = collect_range_rows(
            range(
                &storage,
                root,
                Bound::Unbounded,
                Bound::Unbounded,
                &committed_snapshot(4),
            )
            .unwrap(),
        )
        .unwrap();

        let keys: Vec<Vec<u8>> = rows.into_iter().map(|row| row.key).collect();
        let expected: Vec<Vec<u8>> = [1u32, 3, 5, 7]
            .into_iter()
            .map(|key| key.to_be_bytes().to_vec())
            .collect();
        assert_eq!(keys, expected);
    }

    #[test]
    fn bounded_range_starts_from_non_leftmost_leaf_when_possible() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..256 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &vec![key as u8; 2_400],
                )
                .unwrap(),
            );
        }

        let root = root.unwrap();
        let leftmost = leftmost_leaf_pid(&storage, root).unwrap();
        let (start_pid, slot_idx) = find_start_position(
            &storage,
            root,
            &Bound::Included(200u32.to_be_bytes().to_vec()),
        )
        .unwrap();

        assert_ne!(
            start_pid, leftmost,
            "bounded range should descend to the first relevant leaf"
        );

        let page = storage.read_page(start_pid).unwrap();
        let cell = clustered_leaf::read_cell(&page, slot_idx as u16).unwrap();
        assert_eq!(cell.key, 200u32.to_be_bytes());
    }

    #[test]
    fn range_prefetches_when_advancing_to_next_leaf() {
        let prefetches = Arc::new(AtomicUsize::new(0));
        let mut storage = CountingPrefetchStorage {
            inner: MemoryStorage::new(),
            prefetches: Arc::clone(&prefetches),
        };
        let mut root = None;

        for key in 0u32..64 {
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header(1),
                    &vec![key as u8; 3_000],
                )
                .unwrap(),
            );
        }

        let rows = collect_range_rows(
            range(
                &storage,
                root,
                Bound::Unbounded,
                Bound::Unbounded,
                &committed_snapshot(10),
            )
            .unwrap(),
        )
        .unwrap();

        assert_eq!(rows.len(), 64);
        assert!(
            prefetches.load(Ordering::Relaxed) > 0,
            "cross-leaf scans must issue prefetch hints"
        );
    }

    #[test]
    fn ten_thousand_mixed_rows_stay_sorted() {
        let mut storage = MemoryStorage::new();
        let mut root = None;

        for key in 0u32..10_000 {
            let row_len = 64 + (key as usize % 7) * 113;
            root = Some(
                insert(
                    &mut storage,
                    root,
                    &key.to_be_bytes(),
                    &row_header((key % 17) as u64 + 1),
                    &row_bytes(key, row_len),
                )
                .unwrap(),
            );
        }

        let keys = collect_leaf_chain_keys(&storage, root.unwrap()).unwrap();
        assert_eq!(keys.len(), 10_000);
        for (idx, key) in keys.iter().enumerate() {
            assert_eq!(key.as_slice(), &(idx as u32).to_be_bytes());
        }
    }
}
