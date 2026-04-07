use super::page_utils::{clustered_page_type, reconstruct_row_data};
use super::{ClusteredRangeIter, ClusteredRow, PREFETCH_DEPTH};
use crate::{clustered_leaf, page::PageType, StorageEngine};
use axiomdb_core::{error::DbError, TransactionSnapshot};
use std::ops::Bound;

impl<'a> ClusteredRangeIter<'a> {
    pub(super) fn empty(
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

                let row_data = match reconstruct_row_data(self.storage, &cell) {
                    Ok(row_data) => row_data,
                    Err(err) => return Some(Err(err)),
                };

                return Some(Ok(ClusteredRow {
                    key: cell.key.to_vec(),
                    row_header: cell.row_header,
                    row_data,
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
