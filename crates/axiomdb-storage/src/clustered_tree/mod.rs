//! Clustered B-tree insert path built on top of clustered leaf/internal pages.
//!
//! This module is intentionally storage-first: it provides the first mutable
//! tree controller for clustered pages without yet replacing the heap+index
//! executor path.

use std::ops::Bound;

use axiomdb_core::{error::DbError, TransactionSnapshot};

use crate::{
    clustered_internal, clustered_leaf, clustered_overflow,
    heap::RowHeader,
    page::{Page, PageType},
    StorageEngine,
};

// Private helpers moved to submodules — imported here for the public API functions.
use btree_insert::insert_subtree;
use delete::delete_physical;
use page_utils::{
    bounds_empty, clustered_page_type, materialize_leaf_cell, reconstruct_row_data,
    validate_row_payload, write_page,
};
use search::{descend_to_leaf, find_start_position, leaf_search_checked, leftmost_leaf_pid};

#[derive(Debug, Clone)]
struct OwnedLeafCell {
    key: Vec<u8>,
    row_header: RowHeader,
    total_row_len: usize,
    local_row_data: Vec<u8>,
    overflow_first_page: Option<u64>,
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

enum DeletePhysicalResult {
    NotFound,
    Deleted { underfull: bool, min_changed: bool },
}

#[derive(Debug, Clone)]
pub struct ClusteredRow {
    pub key: Vec<u8>,
    pub row_header: RowHeader,
    pub row_data: Vec<u8>,
}

pub struct RightmostAppendRow<'a> {
    pub key: &'a [u8],
    pub row_header: &'a RowHeader,
    pub row_data: &'a [u8],
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
    validate_row_payload(key, row_data)?;

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
            let cell = materialize_leaf_cell(storage, key, row_header, row_data)?;
            let root_pid = storage.alloc_page(PageType::ClusteredLeaf)?;
            let mut root = Page::new(PageType::ClusteredLeaf, root_pid);
            clustered_leaf::init_clustered_leaf(&mut root);
            clustered_leaf::insert_cell_with_overflow(
                &mut root,
                0,
                &cell.key,
                &cell.row_header,
                cell.total_row_len,
                &cell.local_row_data,
                cell.overflow_first_page,
            )?;
            write_page(storage, root_pid, &mut root)?;
            Ok(root_pid)
        }
    }
}

/// Tries the append-only fast path on a caller-provided rightmost leaf hint.
///
/// This follows the same idea as PostgreSQL/SQLite append bias: only bypass
/// the normal tree descent when the hinted page is still the rightmost leaf
/// and `key` is strictly greater than the last key already stored there.
///
/// Returns `Ok(true)` when the row was inserted directly into the leaf without
/// changing the root. Returns `Ok(false)` when the hint is not usable or the
/// leaf would need a structural split, so the caller must fall back to the
/// normal `insert()` path.
pub fn try_insert_rightmost_leaf(
    storage: &mut dyn StorageEngine,
    hinted_leaf_pid: u64,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<bool, DbError> {
    validate_row_payload(key, row_data)?;

    let page_ref = storage.read_page(hinted_leaf_pid)?;
    if clustered_page_type(&page_ref)? != PageType::ClusteredLeaf {
        return Ok(false);
    }

    let mut page = page_ref.into_page();
    if clustered_leaf::next_leaf(&page) != clustered_leaf::NULL_PAGE {
        return Ok(false);
    }

    let num_cells = clustered_leaf::num_cells(&page) as usize;
    if num_cells == 0 {
        return Ok(false);
    }

    let last_cell = clustered_leaf::read_cell(&page, (num_cells - 1) as u16)?;
    if key <= last_cell.key {
        return Ok(false);
    }

    let cell = materialize_leaf_cell(storage, key, row_header, row_data)?;
    let cleanup_overflow =
        |storage: &mut dyn StorageEngine, first_page: Option<u64>| -> Result<(), DbError> {
            if let Some(first_page) = first_page {
                clustered_overflow::free_chain(storage, first_page)?;
            }
            Ok(())
        };

    match clustered_leaf::insert_cell_with_overflow(
        &mut page,
        num_cells,
        &cell.key,
        &cell.row_header,
        cell.total_row_len,
        &cell.local_row_data,
        cell.overflow_first_page,
    ) {
        Ok(()) => {
            write_page(storage, hinted_leaf_pid, &mut page)?;
            Ok(true)
        }
        Err(DbError::HeapPageFull { .. }) => {
            clustered_leaf::defragment(&mut page);
            match clustered_leaf::insert_cell_with_overflow(
                &mut page,
                num_cells,
                &cell.key,
                &cell.row_header,
                cell.total_row_len,
                &cell.local_row_data,
                cell.overflow_first_page,
            ) {
                Ok(()) => {
                    write_page(storage, hinted_leaf_pid, &mut page)?;
                    Ok(true)
                }
                Err(DbError::HeapPageFull { .. }) => {
                    cleanup_overflow(storage, cell.overflow_first_page)?;
                    Ok(false)
                }
                Err(err) => {
                    cleanup_overflow(storage, cell.overflow_first_page)?;
                    Err(err)
                }
            }
        }
        Err(err) => {
            cleanup_overflow(storage, cell.overflow_first_page)?;
            Err(err)
        }
    }
}

/// Appends as many strictly increasing rows as possible into a hinted
/// rightmost leaf, writing the page once after the batch.
///
/// Returns the number of rows that were appended directly. A return of `0`
/// means the caller must fall back to the normal insert path for the first row.
pub fn try_insert_rightmost_leaf_batch(
    storage: &mut dyn StorageEngine,
    hinted_leaf_pid: u64,
    rows: &[RightmostAppendRow<'_>],
) -> Result<usize, DbError> {
    if rows.is_empty() {
        return Ok(0);
    }

    let page_ref = storage.read_page(hinted_leaf_pid)?;
    if clustered_page_type(&page_ref)? != PageType::ClusteredLeaf {
        return Ok(0);
    }

    let mut page = page_ref.into_page();
    if clustered_leaf::next_leaf(&page) != clustered_leaf::NULL_PAGE {
        return Ok(0);
    }

    let num_cells = clustered_leaf::num_cells(&page) as usize;
    if num_cells == 0 {
        return Ok(0);
    }

    let mut prev_key = clustered_leaf::read_cell(&page, (num_cells - 1) as u16)?
        .key
        .to_vec();
    let mut inserted = 0usize;
    let mut defragmented = false;

    for row in rows {
        validate_row_payload(row.key, row.row_data)?;
        if row.key <= prev_key.as_slice() {
            break;
        }

        let cell = materialize_leaf_cell(storage, row.key, row.row_header, row.row_data)?;
        let insert_pos = clustered_leaf::num_cells(&page) as usize;

        match clustered_leaf::insert_cell_with_overflow(
            &mut page,
            insert_pos,
            &cell.key,
            &cell.row_header,
            cell.total_row_len,
            &cell.local_row_data,
            cell.overflow_first_page,
        ) {
            Ok(()) => {
                inserted += 1;
                prev_key.clear();
                prev_key.extend_from_slice(row.key);
            }
            Err(DbError::HeapPageFull { .. }) if !defragmented => {
                clustered_leaf::defragment(&mut page);
                defragmented = true;
                match clustered_leaf::insert_cell_with_overflow(
                    &mut page,
                    insert_pos,
                    &cell.key,
                    &cell.row_header,
                    cell.total_row_len,
                    &cell.local_row_data,
                    cell.overflow_first_page,
                ) {
                    Ok(()) => {
                        inserted += 1;
                        prev_key.clear();
                        prev_key.extend_from_slice(row.key);
                    }
                    Err(DbError::HeapPageFull { .. }) => {
                        if let Some(first_page) = cell.overflow_first_page {
                            clustered_overflow::free_chain(storage, first_page)?;
                        }
                        break;
                    }
                    Err(err) => {
                        if let Some(first_page) = cell.overflow_first_page {
                            clustered_overflow::free_chain(storage, first_page)?;
                        }
                        return Err(err);
                    }
                }
            }
            Err(DbError::HeapPageFull { .. }) => {
                if let Some(first_page) = cell.overflow_first_page {
                    clustered_overflow::free_chain(storage, first_page)?;
                }
                break;
            }
            Err(err) => {
                if let Some(first_page) = cell.overflow_first_page {
                    clustered_overflow::free_chain(storage, first_page)?;
                }
                return Err(err);
            }
        }
    }

    if inserted > 0 {
        write_page(storage, hinted_leaf_pid, &mut page)?;
    }

    Ok(inserted)
}

/// Looks up one full row by primary key in a clustered B-tree and returns the
/// current inline version when it is visible to `snapshot`.
pub fn lookup(
    storage: &dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    snapshot: &TransactionSnapshot,
) -> Result<Option<ClusteredRow>, DbError> {
    let Some(row) = lookup_physical(storage, root_pid, key)? else {
        return Ok(None);
    };
    if !row.row_header.is_visible(snapshot) {
        return Ok(None);
    }
    Ok(Some(row))
}

/// Looks up the current physical row for `key` regardless of MVCC visibility.
pub fn lookup_physical(
    storage: &dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
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

    let row_data = reconstruct_row_data(storage, &cell)?;

    Ok(Some(ClusteredRow {
        key: cell.key.to_vec(),
        row_header: cell.row_header,
        row_data,
    }))
}

/// Descends from `root_pid` to the owning clustered leaf for `key`.
///
/// This is exposed for executor-side batching that wants to process multiple
/// clustered keys per leaf without re-running a full row lookup each time.
pub fn descend_to_leaf_pub(
    storage: &dyn StorageEngine,
    root_pid: u64,
    key: &[u8],
) -> Result<crate::PageRef, DbError> {
    descend_to_leaf(storage, root_pid, key)
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
        return Ok(ClusteredRangeIter::empty(
            storage,
            from,
            to,
            snapshot.clone(),
        ));
    }

    let Some(root_pid) = root_pid else {
        return Ok(ClusteredRangeIter::empty(
            storage,
            from,
            to,
            snapshot.clone(),
        ));
    };

    let (current_pid, slot_idx) = find_start_position(storage, root_pid, &from)?;

    Ok(ClusteredRangeIter {
        storage,
        current_pid,
        next_leaf_cache: clustered_leaf::NULL_PAGE,
        slot_idx,
        from,
        to,
        snapshot: snapshot.clone(),
        done: false,
    })
}

/// Callback-based full scan over all visible clustered rows — zero-allocation hot path.
///
/// For each visible row, the callback receives:
/// - `inline_data: &[u8]`  — the portion of row_data stored inline in the leaf.
///   For rows without overflow this is the complete row_data. When `overflow` is
///   `None` the caller can decode directly from this slice without any copy.
/// - `overflow: Option<(u64, usize)>` — `Some((first_page, tail_len))` when the row
///   spills to an overflow chain. The caller must call
///   `clustered_overflow::read_chain(storage, first_page, tail_len)` and prepend
///   `inline_data` to reconstruct the full row bytes.
///
/// Unlike [`range`], this path never calls `reconstruct_row_data` and never
/// allocates `cell.key.to_vec()`, saving 2 heap allocations per row on full scans.
pub fn scan_all_callback<F>(
    storage: &dyn StorageEngine,
    root_pid: Option<u64>,
    snapshot: &TransactionSnapshot,
    mut f: F,
) -> Result<(), DbError>
where
    F: FnMut(&[u8], Option<(u64, usize)>) -> Result<(), DbError>,
{
    let Some(root_pid) = root_pid else {
        return Ok(());
    };

    let mut current_pid = leftmost_leaf_pid(storage, root_pid)?;
    let mut next_leaf_cache = clustered_leaf::NULL_PAGE;

    while current_pid != clustered_leaf::NULL_PAGE {
        let page = storage.read_page(current_pid)?;

        match clustered_page_type(&page)? {
            PageType::ClusteredLeaf => {}
            other => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!(
                        "scan_all_callback expected leaf at page {current_pid}, found {other:?}"
                    ),
                });
            }
        }

        if next_leaf_cache == clustered_leaf::NULL_PAGE {
            next_leaf_cache = clustered_leaf::next_leaf(&page);
        }

        let num_cells = clustered_leaf::num_cells(&page) as usize;
        for idx in 0..num_cells {
            let cell = clustered_leaf::read_cell(&page, idx as u16)?;
            if !cell.row_header.is_visible(snapshot) {
                continue;
            }
            let overflow = match (
                cell.total_row_len.cmp(&cell.row_data.len()),
                cell.overflow_first_page,
            ) {
                (std::cmp::Ordering::Greater, Some(fp)) => {
                    Some((fp, cell.total_row_len - cell.row_data.len()))
                }
                _ => None,
            };
            f(cell.row_data, overflow)?;
        }

        let next_pid = next_leaf_cache;
        if next_pid != clustered_leaf::NULL_PAGE {
            storage.prefetch_hint(next_pid, PREFETCH_DEPTH);
        }
        current_pid = next_pid;
        next_leaf_cache = clustered_leaf::NULL_PAGE;
    }

    Ok(())
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
    validate_row_payload(key, new_row_data)?;

    let Some(root_pid) = root_pid else {
        return Ok(false);
    };

    let leaf_ref = descend_to_leaf(storage, root_pid, key)?;
    let pos = match leaf_search_checked(&leaf_ref, key)? {
        Ok(pos) => pos,
        Err(_) => return Ok(false),
    };

    let (old_header, old_total_row_len, old_overflow_first_page) = {
        let cell = clustered_leaf::read_cell(&leaf_ref, pos as u16)?;
        if !cell.row_header.is_visible(snapshot) {
            return Ok(false);
        }
        (
            cell.row_header,
            cell.total_row_len,
            cell.overflow_first_page,
        )
    };

    let new_header = RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version: old_header.row_version.saturating_add(1),
        _flags: old_header._flags,
    };
    let new_cell = materialize_leaf_cell(storage, key, &new_header, new_row_data)?;

    let mut page = leaf_ref.into_page();
    let page_id = page.header().page_id;
    let available = clustered_leaf::free_space(&page)
        + clustered_leaf::cell_footprint(key.len(), old_total_row_len);
    let needed = clustered_leaf::cell_footprint(key.len(), new_row_data.len());

    match clustered_leaf::rewrite_cell_same_key_with_overflow(
        &mut page,
        pos,
        key,
        &new_header,
        new_cell.total_row_len,
        &new_cell.local_row_data,
        new_cell.overflow_first_page,
    )? {
        Some(_) => {
            write_page(storage, page_id, &mut page)?;
            if let Some(first_page) = old_overflow_first_page {
                clustered_overflow::free_chain(storage, first_page)?;
            }
            Ok(true)
        }
        None => {
            if let Some(first_page) = new_cell.overflow_first_page {
                clustered_overflow::free_chain(storage, first_page)?;
            }
            Err(DbError::HeapPageFull {
                page_id,
                needed,
                available,
            })
        }
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

    let (old_header, old_total_row_len, old_local_row_data, old_overflow_first_page) = {
        let cell = clustered_leaf::read_cell(&leaf_ref, pos as u16)?;
        if !cell.row_header.is_visible(snapshot) {
            return Ok(false);
        }
        (
            cell.row_header,
            cell.total_row_len,
            cell.row_data.to_vec(),
            cell.overflow_first_page,
        )
    };

    let mut page = leaf_ref.into_page();
    let page_id = page.header().page_id;
    let new_header = RowHeader {
        txn_id_created: old_header.txn_id_created,
        txn_id_deleted: txn_id,
        row_version: old_header.row_version,
        _flags: old_header._flags,
    };

    match clustered_leaf::rewrite_cell_same_key_with_overflow(
        &mut page,
        pos,
        key,
        &new_header,
        old_total_row_len,
        &old_local_row_data,
        old_overflow_first_page,
    )? {
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

/// Rewrites one clustered row, falling back to structural delete+insert when
/// same-leaf growth no longer fits in the owning page.
pub fn update_with_relocation(
    storage: &mut dyn StorageEngine,
    root_pid: Option<u64>,
    key: &[u8],
    new_row_data: &[u8],
    txn_id: u64,
    snapshot: &TransactionSnapshot,
) -> Result<Option<u64>, DbError> {
    validate_row_payload(key, new_row_data)?;

    let Some(root_pid) = root_pid else {
        return Ok(None);
    };

    match update_in_place(storage, Some(root_pid), key, new_row_data, txn_id, snapshot) {
        Ok(true) => return Ok(Some(root_pid)),
        Ok(false) => return Ok(None),
        Err(DbError::HeapPageFull { .. }) => {}
        Err(err) => return Err(err),
    }

    let Some(old_row) = lookup(storage, Some(root_pid), key, snapshot)? else {
        return Ok(None);
    };

    let (deleted, root_after_delete) = delete_physical(storage, root_pid, key)?;
    if !deleted {
        return Ok(None);
    }

    let new_header = RowHeader {
        txn_id_created: txn_id,
        txn_id_deleted: 0,
        row_version: old_row.row_header.row_version.saturating_add(1),
        _flags: old_row.row_header._flags,
    };
    let new_root = insert(
        storage,
        Some(root_after_delete),
        key,
        &new_header,
        new_row_data,
    )?;
    Ok(Some(new_root))
}

/// Physically removes the current clustered row for `key`, if present.
///
/// This helper is intended for rollback/undo paths that must remove the current
/// version regardless of MVCC visibility rules.
pub fn delete_physical_by_key(
    storage: &mut dyn StorageEngine,
    root_pid: u64,
    key: &[u8],
) -> Result<Option<u64>, DbError> {
    let (deleted, new_root) = delete_physical(storage, root_pid, key)?;
    if deleted {
        Ok(Some(new_root))
    } else {
        Ok(None)
    }
}

/// Restores the exact clustered row image for `key`.
///
/// If a current version of `key` exists, it is physically removed first. The
/// replacement row is then inserted with the provided `RowHeader` and logical
/// row bytes, allocating a fresh overflow chain when needed.
pub fn restore_exact_row_image(
    storage: &mut dyn StorageEngine,
    root_pid: u64,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<u64, DbError> {
    validate_row_payload(key, row_data)?;
    let root_after_delete = match delete_physical_by_key(storage, root_pid, key)? {
        Some(new_root) => new_root,
        None => root_pid,
    };
    insert(storage, Some(root_after_delete), key, row_header, row_data)
}

mod btree_insert;
mod delete;
mod iter;
mod page_utils;
mod search;
#[cfg(test)]
mod tests;
