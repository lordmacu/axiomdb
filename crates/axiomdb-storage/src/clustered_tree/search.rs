use super::page_utils::clustered_page_type;
use crate::{
    clustered_internal, clustered_leaf,
    page::{Page, PageType},
    StorageEngine,
};
use axiomdb_core::error::DbError;
use std::ops::Bound;

pub(super) fn descend_to_leaf(
    storage: &dyn StorageEngine,
    root_pid: u64,
    key: &[u8],
) -> Result<crate::PageRef, DbError> {
    let locks = storage.page_lock_table();
    'restart: loop {
        let mut pid = root_pid;
        let mut current_guard = locks.read(pid);
        loop {
            let page = storage.read_page(pid)?;
            match clustered_page_type(&page)? {
                PageType::ClusteredLeaf => {
                    drop(current_guard);
                    return Ok(page);
                }
                PageType::ClusteredInternal => {
                    let child_idx = clustered_internal::find_child_idx(&page, key)?;
                    let child_pid = clustered_internal::child_at(&page, child_idx as u16)?;
                    drop(page);
                    let Some(child_guard) = locks.try_read(child_pid) else {
                        drop(current_guard);
                        continue 'restart;
                    };
                    drop(current_guard);
                    current_guard = child_guard;
                    pid = child_pid;
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
}

pub(super) fn leftmost_leaf_pid(
    storage: &dyn StorageEngine,
    root_pid: u64,
) -> Result<u64, DbError> {
    let locks = storage.page_lock_table();
    'restart: loop {
        let mut pid = root_pid;
        let mut current_guard = locks.read(pid);
        loop {
            let page = storage.read_page(pid)?;
            match clustered_page_type(&page)? {
                PageType::ClusteredLeaf => {
                    drop(current_guard);
                    return Ok(pid);
                }
                PageType::ClusteredInternal => {
                    let child_pid = clustered_internal::child_at(&page, 0)?;
                    drop(page);
                    let Some(child_guard) = locks.try_read(child_pid) else {
                        drop(current_guard);
                        continue 'restart;
                    };
                    drop(current_guard);
                    current_guard = child_guard;
                    pid = child_pid;
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
}

pub(super) fn find_start_position(
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

pub(super) fn leaf_search_checked(
    page: &Page,
    key: &[u8],
) -> Result<Result<usize, usize>, DbError> {
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
