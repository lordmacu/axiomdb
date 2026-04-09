use super::delete::{rebalance_internal_pair, rebalance_leaf_pair};
use super::page_utils::rebuild_internal_page;
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

fn collect_leaf_chain_pids(
    storage: &dyn StorageEngine,
    root_pid: u64,
) -> Result<Vec<u64>, DbError> {
    let mut leaf_pid = leftmost_leaf_pid(storage, root_pid)?;
    let mut pids = Vec::new();

    loop {
        let page = storage.read_page(leaf_pid)?;
        assert_eq!(clustered_page_type(&page)?, PageType::ClusteredLeaf);
        pids.push(leaf_pid);

        let next = clustered_leaf::next_leaf(&page);
        if next == clustered_leaf::NULL_PAGE {
            break;
        }
        leaf_pid = next;
    }

    Ok(pids)
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

    fn write_page(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        self.inner.write_page(page_id, page)
    }

    fn write_page_under_page_lock(&self, page_id: u64, page: &Page) -> Result<(), DbError> {
        self.inner.write_page_under_page_lock(page_id, page)
    }

    fn alloc_page(&self, page_type: PageType) -> Result<u64, DbError> {
        self.inner.alloc_page(page_type)
    }

    fn free_page(&self, page_id: u64) -> Result<(), DbError> {
        self.inner.free_page(page_id)
    }

    fn flush(&self) -> Result<(), DbError> {
        self.inner.flush()
    }

    fn page_count(&self) -> u64 {
        self.inner.page_count()
    }

    fn prefetch_hint(&self, start_page_id: u64, count: u64) {
        self.prefetches.fetch_add(1, Ordering::Relaxed);
        self.inner.prefetch_hint(start_page_id, count);
    }

    fn page_lock_table(&self) -> &crate::page_lock::PageLockTable {
        self.inner.page_lock_table()
    }
}

include!("tests_insert.rs");
include!("tests_lookup.rs");
include!("tests_update.rs");
include!("tests_delete.rs");
include!("tests_range.rs");
