use super::page_utils::{
    child_is_safe_for_insert, choose_internal_promotion_idx, choose_leaf_split_idx,
    clustered_page_type, collect_internal_cells, collect_leaf_cells, materialize_leaf_cell,
    rebuild_internal_page, rebuild_leaf_page, write_page,
};
use super::search::leaf_search_checked;
use super::{InsertResult, OwnedInternalCell, OwnedLeafCell};
use crate::{
    clustered_internal, clustered_leaf,
    heap::RowHeader,
    local_page_batch::{batch_alloc_page, LocalPageBatch},
    page::{Page, PageType},
    StorageEngine,
};
use axiomdb_core::error::DbError;

#[allow(clippy::needless_option_as_deref)]
pub(super) fn insert_subtree(
    storage: &mut dyn StorageEngine,
    batch: Option<&mut LocalPageBatch>,
    pid: u64,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<InsertResult, DbError> {
    let parent_guard = storage.page_lock_table().write(pid);
    let page_ref = storage.read_page(pid)?;
    match clustered_page_type(&page_ref)? {
        PageType::ClusteredLeaf => insert_into_leaf(
            storage,
            batch,
            pid,
            page_ref.into_page(),
            key,
            row_header,
            row_data,
        ),
        PageType::ClusteredInternal => {
            let page = page_ref.into_page();

            // Phase 40.8c: early X-latch release on safe descent.
            let child_idx = clustered_internal::find_child_idx(&page, key)?;
            let child_pid = clustered_internal::child_at(&page, child_idx as u16)?;
            if child_is_safe_for_insert(storage, child_pid, key, row_data.len())? {
                drop(parent_guard);
                let result = insert_subtree(storage, batch, child_pid, key, row_header, row_data)?;
                debug_assert!(
                    matches!(result, InsertResult::Inserted),
                    "safe child must return InsertResult::Inserted"
                );
                return Ok(InsertResult::Inserted);
            }

            insert_into_internal(storage, batch, pid, page, key, row_header, row_data)
        }
        other => Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered tree encountered unsupported page type {other:?} at page {pid}"
            ),
        }),
    }
}

#[allow(clippy::needless_option_as_deref)]
fn insert_into_leaf(
    storage: &mut dyn StorageEngine,
    mut batch: Option<&mut LocalPageBatch>,
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

    let cell = materialize_leaf_cell(storage, batch.as_deref_mut(), key, row_header, row_data)?;

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
            page.update_checksum();
            storage.write_page_under_page_lock(pid, &page)?;
            Ok(InsertResult::Inserted)
        }
        Err(DbError::HeapPageFull { .. }) => {
            clustered_leaf::defragment(&mut page);
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
                    page.update_checksum();
                    storage.write_page_under_page_lock(pid, &page)?;
                    Ok(InsertResult::Inserted)
                }
                Err(DbError::HeapPageFull { .. }) => {
                    split_leaf(storage, batch, pid, &page, insert_pos, cell)
                }
                Err(err) => Err(err),
            }
        }
        Err(err) => Err(err),
    }
}

fn split_leaf(
    storage: &mut dyn StorageEngine,
    batch: Option<&mut LocalPageBatch>,
    pid: u64,
    page: &Page,
    insert_pos: usize,
    cell: OwnedLeafCell,
) -> Result<InsertResult, DbError> {
    let mut cells = collect_leaf_cells(page)?;
    cells.insert(insert_pos, cell);

    let split_at = choose_leaf_split_idx(&cells);
    let old_next_leaf = clustered_leaf::next_leaf(page);
    let right_pid = batch_alloc_page(storage, batch, PageType::ClusteredLeaf)?;

    let mut left_page = Page::new(PageType::ClusteredLeaf, pid);
    rebuild_leaf_page(&mut left_page, &cells[..split_at], right_pid)?;
    let mut right_page = Page::new(PageType::ClusteredLeaf, right_pid);
    rebuild_leaf_page(&mut right_page, &cells[split_at..], old_next_leaf)?;

    left_page.update_checksum();
    storage.write_page_under_page_lock(pid, &left_page)?;
    write_page(storage, right_pid, &mut right_page)?;

    Ok(InsertResult::Split {
        sep_key: cells[split_at].key.clone(),
        right_pid,
    })
}

#[allow(clippy::needless_option_as_deref)]
fn insert_into_internal(
    storage: &mut dyn StorageEngine,
    mut batch: Option<&mut LocalPageBatch>,
    pid: u64,
    mut page: Page,
    key: &[u8],
    row_header: &RowHeader,
    row_data: &[u8],
) -> Result<InsertResult, DbError> {
    let child_idx = clustered_internal::find_child_idx(&page, key)?;
    let child_pid = clustered_internal::child_at(&page, child_idx as u16)?;

    match insert_subtree(
        storage,
        batch.as_deref_mut(),
        child_pid,
        key,
        row_header,
        row_data,
    )? {
        InsertResult::Inserted => Ok(InsertResult::Inserted),
        InsertResult::Split { sep_key, right_pid } => {
            match clustered_internal::insert_at(&mut page, child_idx, &sep_key, right_pid) {
                Ok(()) => {
                    page.update_checksum();
                    storage.write_page_under_page_lock(pid, &page)?;
                    Ok(InsertResult::Inserted)
                }
                Err(DbError::HeapPageFull { .. }) => {
                    clustered_internal::defragment(&mut page);
                    match clustered_internal::insert_at(&mut page, child_idx, &sep_key, right_pid) {
                        Ok(()) => {
                            page.update_checksum();
                            storage.write_page_under_page_lock(pid, &page)?;
                            Ok(InsertResult::Inserted)
                        }
                        Err(DbError::HeapPageFull { .. }) => split_internal(
                            storage, batch, pid, &page, child_idx, &sep_key, right_pid,
                        ),
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
    batch: Option<&mut LocalPageBatch>,
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
    let new_right_pid = batch_alloc_page(storage, batch, PageType::ClusteredInternal)?;

    let mut left_page = Page::new(PageType::ClusteredInternal, pid);
    rebuild_internal_page(&mut left_page, leftmost_child, &separators[..promoted_idx])?;

    let mut right_page = Page::new(PageType::ClusteredInternal, new_right_pid);
    rebuild_internal_page(
        &mut right_page,
        promoted.right_child,
        &separators[promoted_idx + 1..],
    )?;

    left_page.update_checksum();
    storage.write_page_under_page_lock(pid, &left_page)?;
    write_page(storage, new_right_pid, &mut right_page)?;

    Ok(InsertResult::Split {
        sep_key: promoted.key,
        right_pid: new_right_pid,
    })
}
