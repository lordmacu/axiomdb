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

pub(super) fn leftmost_leaf_pid(storage: &dyn StorageEngine, mut pid: u64) -> Result<u64, DbError> {
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
