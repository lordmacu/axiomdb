use super::page_utils::{
    child_is_safe_for_delete, choose_internal_rebalance_promotion_idx, choose_leaf_rebalance_idx,
    clustered_page_type, collect_internal_cells, collect_leaf_cells, internal_footprint,
    internal_underfull, leaf_footprint, leaf_underfull, rebuild_internal_page, rebuild_leaf_page,
};
use super::search::leaf_search_checked;
use super::{DeletePhysicalResult, OwnedInternalCell};
use crate::{
    clustered_internal, clustered_leaf, clustered_overflow,
    page::{Page, PageType},
    StorageEngine,
};
use axiomdb_core::error::DbError;

pub(super) fn delete_physical(
    storage: &mut dyn StorageEngine,
    root_pid: u64,
    key: &[u8],
) -> Result<(bool, u64), DbError> {
    match delete_physical_subtree(storage, root_pid, key, true)? {
        DeletePhysicalResult::NotFound => Ok((false, root_pid)),
        DeletePhysicalResult::Deleted { .. } => {
            let new_root = collapse_root(storage, root_pid)?;
            Ok((true, new_root))
        }
    }
}

fn delete_physical_subtree(
    storage: &mut dyn StorageEngine,
    pid: u64,
    key: &[u8],
    is_root: bool,
) -> Result<DeletePhysicalResult, DbError> {
    let parent_guard = storage.page_lock_table().write(pid);
    let page_ref = storage.read_page(pid)?;
    match clustered_page_type(&page_ref)? {
        PageType::ClusteredLeaf => {
            delete_physical_from_leaf(storage, pid, page_ref.into_page(), key, is_root)
        }
        PageType::ClusteredInternal => {
            let page = page_ref.into_page();

            // Phase 40.8c: early X-latch release on safe descent.
            // We peek at the immediate child while still holding our parent
            // X-latch. If the child is comfortably above the underfull
            // threshold (and is a leaf, so we know its full safety story),
            // the recursive delete cannot return `underfull = true` and the
            // parent never needs an update. Drop the parent latch before
            // recursing so concurrent readers can resume on this page.
            let child_idx = clustered_internal::find_child_idx(&page, key)?;
            let child_pid = clustered_internal::child_at(&page, child_idx as u16)?;
            if child_is_safe_for_delete(storage, child_pid)? {
                drop(parent_guard);
                let result = delete_physical_subtree(storage, child_pid, key, false)?;
                return match result {
                    DeletePhysicalResult::NotFound => Ok(DeletePhysicalResult::NotFound),
                    DeletePhysicalResult::Deleted {
                        underfull,
                        min_changed,
                    } => {
                        debug_assert!(
                            !underfull,
                            "safe clustered child must not return underfull from delete"
                        );
                        // The min key of the child may still have changed,
                        // but the parent only needs separator repair when
                        // child_idx > 0. Re-acquire the parent latch only in
                        // that branch so the safe path stays cheap when the
                        // descent went through the leftmost child.
                        if min_changed && child_idx > 0 {
                            let _re_guard = storage.page_lock_table().write(pid);
                            let mut page = storage.read_page(pid)?.into_page();
                            match repair_separator_for_child(storage, &mut page, child_idx) {
                                Ok(()) => {
                                    page.update_checksum();
                                    storage.write_page_under_page_lock(pid, &page)?;
                                }
                                Err(DbError::HeapPageFull { .. }) => {
                                    // Same fallback semantics as the
                                    // pessimistic path: a separator that grew
                                    // beyond the parent budget forces an
                                    // upward rebalance signal. We must report
                                    // `underfull = true` so the grandparent
                                    // can deal with it. The grandparent has
                                    // already lost our latch, so the only
                                    // safe move is to write the page out and
                                    // surface the underfull flag now.
                                    page.update_checksum();
                                    storage.write_page_under_page_lock(pid, &page)?;
                                    return Ok(DeletePhysicalResult::Deleted {
                                        underfull: true,
                                        min_changed: false,
                                    });
                                }
                                Err(err) => return Err(err),
                            }
                        }
                        // The parent's own min_changed only matters when this
                        // child was the leftmost. Forward that signal up.
                        let min_changed_self = child_idx == 0 && min_changed;
                        Ok(DeletePhysicalResult::Deleted {
                            underfull: false,
                            min_changed: min_changed_self,
                        })
                    }
                };
            }

            delete_physical_from_internal(storage, pid, page, key, is_root)
        }
        other => Err(DbError::BTreeCorrupted {
            msg: format!("clustered physical delete encountered unsupported page type {other:?}"),
        }),
    }
}

fn delete_physical_from_leaf(
    storage: &mut dyn StorageEngine,
    pid: u64,
    mut page: Page,
    key: &[u8],
    is_root: bool,
) -> Result<DeletePhysicalResult, DbError> {
    let pos = match leaf_search_checked(&page, key)? {
        Ok(pos) => pos,
        Err(_) => return Ok(DeletePhysicalResult::NotFound),
    };

    let overflow_first_page = clustered_leaf::read_cell(&page, pos as u16)?.overflow_first_page;
    let min_changed = pos == 0;
    clustered_leaf::remove_cell(&mut page, pos)?;
    page.update_checksum();
    storage.write_page_under_page_lock(pid, &page)?;
    if let Some(first_page) = overflow_first_page {
        clustered_overflow::free_chain(storage, first_page)?;
    }

    Ok(DeletePhysicalResult::Deleted {
        underfull: !is_root && leaf_underfull(&page),
        min_changed,
    })
}

fn delete_physical_from_internal(
    storage: &mut dyn StorageEngine,
    pid: u64,
    page: Page,
    key: &[u8],
    is_root: bool,
) -> Result<DeletePhysicalResult, DbError> {
    let child_idx = clustered_internal::find_child_idx(&page, key)?;
    let child_pid = clustered_internal::child_at(&page, child_idx as u16)?;

    match delete_physical_subtree(storage, child_pid, key, false)? {
        DeletePhysicalResult::NotFound => Ok(DeletePhysicalResult::NotFound),
        DeletePhysicalResult::Deleted {
            underfull,
            min_changed,
        } => {
            let mut page = storage.read_page(pid)?.into_page();

            if underfull {
                rebalance_child(storage, &mut page, child_idx)?;
            } else if min_changed && child_idx > 0 {
                match repair_separator_for_child(storage, &mut page, child_idx) {
                    Ok(()) => {}
                    Err(DbError::HeapPageFull { .. }) => {
                        // The new separator is longer than the old one and the
                        // internal page cannot accommodate it.  Force a
                        // rebalance of this internal page at the next level up
                        // by reporting it as underfull.  This is extremely rare
                        // (near-full internal page + key growth during delete
                        // rebalance).
                        let min_changed_self = child_idx == 0 && min_changed;
                        page.update_checksum();
                        storage.write_page_under_page_lock(pid, &page)?;
                        return Ok(DeletePhysicalResult::Deleted {
                            underfull: true, // force parent rebalance
                            min_changed: min_changed_self,
                        });
                    }
                    Err(err) => return Err(err),
                }
            }

            let min_changed_self = child_idx == 0 && min_changed;
            let underfull_self = !is_root && internal_underfull(&page);
            page.update_checksum();
            storage.write_page_under_page_lock(pid, &page)?;

            Ok(DeletePhysicalResult::Deleted {
                underfull: underfull_self,
                min_changed: min_changed_self,
            })
        }
    }
}

fn rebalance_child(
    storage: &mut dyn StorageEngine,
    parent: &mut Page,
    child_idx: usize,
) -> Result<(), DbError> {
    let child_pid = clustered_internal::child_at(parent, child_idx as u16)?;
    let child = storage.read_page(child_pid)?;
    match clustered_page_type(&child)? {
        PageType::ClusteredLeaf => rebalance_leaf_child(storage, parent, child_idx),
        PageType::ClusteredInternal => rebalance_internal_child(storage, parent, child_idx),
        other => Err(DbError::BTreeCorrupted {
            msg: format!("clustered rebalance expected clustered child, found {other:?}"),
        }),
    }
}

fn rebalance_leaf_child(
    storage: &mut dyn StorageEngine,
    parent: &mut Page,
    child_idx: usize,
) -> Result<(), DbError> {
    if child_idx > 0 {
        let left_pid = clustered_internal::child_at(parent, (child_idx - 1) as u16)?;
        let child_pid = clustered_internal::child_at(parent, child_idx as u16)?;
        return rebalance_leaf_pair(storage, parent, child_idx - 1, left_pid, child_pid);
    }

    let right_pid = clustered_internal::child_at(parent, (child_idx + 1) as u16)?;
    let child_pid = clustered_internal::child_at(parent, child_idx as u16)?;
    rebalance_leaf_pair(storage, parent, child_idx, child_pid, right_pid)
}

pub(super) fn rebalance_leaf_pair(
    storage: &mut dyn StorageEngine,
    parent: &mut Page,
    sep_idx: usize,
    left_pid: u64,
    right_pid: u64,
) -> Result<(), DbError> {
    let (_guard_a, _guard_b) = if left_pid <= right_pid {
        (
            storage.page_lock_table().write(left_pid),
            storage.page_lock_table().write(right_pid),
        )
    } else {
        (
            storage.page_lock_table().write(right_pid),
            storage.page_lock_table().write(left_pid),
        )
    };
    let left = storage.read_page(left_pid)?;
    let right = storage.read_page(right_pid)?;

    if clustered_page_type(&left)? != PageType::ClusteredLeaf
        || clustered_page_type(&right)? != PageType::ClusteredLeaf
    {
        return Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered leaf rebalance expected leaf siblings at {left_pid} and {right_pid}"
            ),
        });
    }

    let mut cells = collect_leaf_cells(&left)?;
    cells.extend(collect_leaf_cells(&right)?);
    let right_next = clustered_leaf::next_leaf(&right);

    if leaf_footprint(&cells) <= clustered_leaf::page_capacity_bytes() {
        let mut left_page = Page::new(PageType::ClusteredLeaf, left_pid);
        rebuild_leaf_page(&mut left_page, &cells, right_next)?;
        left_page.update_checksum();
        storage.write_page_under_page_lock(left_pid, &left_page)?;
        storage.free_page(right_pid)?;
        clustered_internal::remove_at(parent, sep_idx, sep_idx + 1)?;
        return Ok(());
    }

    let split_at = choose_leaf_rebalance_idx(&cells)?;

    // Pre-check: verify the new separator fits in the parent page before
    // committing any leaf page writes.  If it doesn't fit, fall back to
    // merge — even if slightly overfull — to avoid leaving the parent in an
    // inconsistent state.
    let new_sep_key = &cells[split_at].key;
    let sep_fits = {
        let current_sep = clustered_internal::key_at(parent, sep_idx as u16)?;
        if new_sep_key.len() <= current_sep.len() {
            true // new sep is same size or smaller — always fits
        } else {
            let growth = new_sep_key.len() - current_sep.len();
            clustered_internal::free_space(parent) >= growth || {
                // Try defrag to reclaim space.
                clustered_internal::defragment(parent);
                clustered_internal::free_space(parent) >= growth
            }
        }
    };

    if !sep_fits {
        // Separator growth exceeds parent budget.  Force merge instead:
        // combine all cells into the left page, free right, remove separator.
        // The left page may be slightly fuller than ideal, but structurally
        // correct.  The next insert or rebalance pass will split it if needed.
        let mut left_page = Page::new(PageType::ClusteredLeaf, left_pid);
        rebuild_leaf_page(&mut left_page, &cells, right_next)?;
        left_page.update_checksum();
        storage.write_page_under_page_lock(left_pid, &left_page)?;
        storage.free_page(right_pid)?;
        clustered_internal::remove_at(parent, sep_idx, sep_idx + 1)?;
        return Ok(());
    }

    let mut left_page = Page::new(PageType::ClusteredLeaf, left_pid);
    rebuild_leaf_page(&mut left_page, &cells[..split_at], right_pid)?;
    let mut right_page = Page::new(PageType::ClusteredLeaf, right_pid);
    rebuild_leaf_page(&mut right_page, &cells[split_at..], right_next)?;

    left_page.update_checksum();
    storage.write_page_under_page_lock(left_pid, &left_page)?;
    right_page.update_checksum();
    storage.write_page_under_page_lock(right_pid, &right_page)?;
    rewrite_separator_at(parent, sep_idx, new_sep_key)?;
    Ok(())
}

fn rebalance_internal_child(
    storage: &mut dyn StorageEngine,
    parent: &mut Page,
    child_idx: usize,
) -> Result<(), DbError> {
    if child_idx > 0 {
        let left_pid = clustered_internal::child_at(parent, (child_idx - 1) as u16)?;
        let child_pid = clustered_internal::child_at(parent, child_idx as u16)?;
        return rebalance_internal_pair(storage, parent, child_idx - 1, left_pid, child_pid);
    }

    let right_pid = clustered_internal::child_at(parent, (child_idx + 1) as u16)?;
    let child_pid = clustered_internal::child_at(parent, child_idx as u16)?;
    rebalance_internal_pair(storage, parent, child_idx, child_pid, right_pid)
}

pub(super) fn rebalance_internal_pair(
    storage: &mut dyn StorageEngine,
    parent: &mut Page,
    sep_idx: usize,
    left_pid: u64,
    right_pid: u64,
) -> Result<(), DbError> {
    let (_guard_a, _guard_b) = if left_pid <= right_pid {
        (
            storage.page_lock_table().write(left_pid),
            storage.page_lock_table().write(right_pid),
        )
    } else {
        (
            storage.page_lock_table().write(right_pid),
            storage.page_lock_table().write(left_pid),
        )
    };
    let left = storage.read_page(left_pid)?;
    let right = storage.read_page(right_pid)?;

    if clustered_page_type(&left)? != PageType::ClusteredInternal
        || clustered_page_type(&right)? != PageType::ClusteredInternal
    {
        return Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered internal rebalance expected internal siblings at {left_pid} and {right_pid}"
            ),
        });
    }

    let bridge_key = clustered_internal::key_at(parent, sep_idx as u16)?.to_vec();
    let leftmost_child = clustered_internal::leftmost_child(&left);
    let mut separators = collect_internal_cells(&left)?;
    separators.push(OwnedInternalCell {
        key: bridge_key,
        right_child: clustered_internal::leftmost_child(&right),
    });
    separators.extend(collect_internal_cells(&right)?);

    if internal_footprint(&separators) <= clustered_internal::page_capacity_bytes() {
        let mut left_page = Page::new(PageType::ClusteredInternal, left_pid);
        rebuild_internal_page(&mut left_page, leftmost_child, &separators)?;
        left_page.update_checksum();
        storage.write_page_under_page_lock(left_pid, &left_page)?;
        storage.free_page(right_pid)?;
        clustered_internal::remove_at(parent, sep_idx, sep_idx + 1)?;
        return Ok(());
    }

    let promoted_idx = choose_internal_rebalance_promotion_idx(&separators)?;
    let promoted = separators[promoted_idx].clone();

    // Pre-check: verify promoted separator fits in parent before committing
    // child page writes.  If it doesn't fit, force merge to avoid leaving
    // parent in inconsistent state (same strategy as rebalance_leaf_pair).
    let promoted_fits = {
        let current_sep = clustered_internal::key_at(parent, sep_idx as u16)?;
        if promoted.key.len() <= current_sep.len() {
            true
        } else {
            let growth = promoted.key.len() - current_sep.len();
            clustered_internal::free_space(parent) >= growth || {
                clustered_internal::defragment(parent);
                clustered_internal::free_space(parent) >= growth
            }
        }
    };

    if !promoted_fits {
        // Promoted separator exceeds parent budget.  Force merge instead.
        let mut left_page = Page::new(PageType::ClusteredInternal, left_pid);
        rebuild_internal_page(&mut left_page, leftmost_child, &separators)?;
        left_page.update_checksum();
        storage.write_page_under_page_lock(left_pid, &left_page)?;
        storage.free_page(right_pid)?;
        clustered_internal::remove_at(parent, sep_idx, sep_idx + 1)?;
        return Ok(());
    }

    let mut left_page = Page::new(PageType::ClusteredInternal, left_pid);
    rebuild_internal_page(&mut left_page, leftmost_child, &separators[..promoted_idx])?;
    let mut right_page = Page::new(PageType::ClusteredInternal, right_pid);
    rebuild_internal_page(
        &mut right_page,
        promoted.right_child,
        &separators[promoted_idx + 1..],
    )?;

    left_page.update_checksum();
    storage.write_page_under_page_lock(left_pid, &left_page)?;
    right_page.update_checksum();
    storage.write_page_under_page_lock(right_pid, &right_page)?;
    rewrite_separator_at(parent, sep_idx, &promoted.key)?;
    Ok(())
}

fn repair_separator_for_child(
    storage: &dyn StorageEngine,
    parent: &mut Page,
    child_idx: usize,
) -> Result<(), DbError> {
    debug_assert!(child_idx > 0);

    let right_child_pid = clustered_internal::child_at(parent, child_idx as u16)?;
    let Some(new_key) = subtree_min_key(storage, right_child_pid)? else {
        return Err(DbError::BTreeCorrupted {
            msg: format!(
                "clustered separator repair found empty right subtree at child index {child_idx}"
            ),
        });
    };
    rewrite_separator_at(parent, child_idx - 1, &new_key)
}

fn rewrite_separator_at(parent: &mut Page, sep_idx: usize, new_key: &[u8]) -> Result<(), DbError> {
    let current = clustered_internal::key_at(parent, sep_idx as u16)?;
    if current == new_key {
        return Ok(());
    }

    let leftmost_child = clustered_internal::leftmost_child(parent);
    let mut separators = collect_internal_cells(parent)?;
    separators[sep_idx].key = new_key.to_vec();

    // Verify the updated separators fit on a single clean page.
    // A rebuild creates a fresh page, so fragmentation is not the issue —
    // only genuine overflow (total separator footprint > capacity) can fail.
    let total_footprint: usize = separators
        .iter()
        .map(|cell| clustered_internal::separator_footprint(cell.key.len()))
        .sum();
    if total_footprint > clustered_internal::page_capacity_bytes() {
        // The new separator is too large for the current page budget.
        // Remove the old separator and re-insert with the new key via the
        // standard defrag→split path that insert_into_internal already handles.
        // This is extremely rare (requires near-full internal page + key growth).
        let right_child = separators[sep_idx].right_child;
        clustered_internal::remove_at(parent, sep_idx, sep_idx + 1)?;
        clustered_internal::defragment(parent);
        match clustered_internal::insert_at(parent, sep_idx, new_key, right_child) {
            Ok(()) => return Ok(()),
            Err(DbError::HeapPageFull { .. }) => {
                // Even after defrag the separator doesn't fit — this means the
                // internal page genuinely needs a split.  Signal the caller so
                // it can propagate the split upward.
                return Err(DbError::HeapPageFull {
                    page_id: parent.header().page_id,
                    needed: clustered_internal::separator_footprint(new_key.len()),
                    available: clustered_internal::free_space(parent),
                });
            }
            Err(err) => return Err(err),
        }
    }

    let mut rebuilt = Page::new(PageType::ClusteredInternal, parent.header().page_id);
    rebuild_internal_page(&mut rebuilt, leftmost_child, &separators)?;
    *parent = rebuilt;
    Ok(())
}

fn subtree_min_key(storage: &dyn StorageEngine, mut pid: u64) -> Result<Option<Vec<u8>>, DbError> {
    loop {
        let page = storage.read_page(pid)?;
        match clustered_page_type(&page)? {
            PageType::ClusteredLeaf => {
                if clustered_leaf::num_cells(&page) == 0 {
                    return Ok(None);
                }
                return Ok(Some(clustered_leaf::read_cell(&page, 0)?.key.to_vec()));
            }
            PageType::ClusteredInternal => {
                pid = clustered_internal::child_at(&page, 0)?;
            }
            other => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!(
                        "clustered subtree_min_key expected clustered page, found {other:?}"
                    ),
                });
            }
        }
    }
}

fn collapse_root(storage: &mut dyn StorageEngine, mut root_pid: u64) -> Result<u64, DbError> {
    loop {
        let page = storage.read_page(root_pid)?;
        match clustered_page_type(&page)? {
            PageType::ClusteredLeaf => return Ok(root_pid),
            PageType::ClusteredInternal => {
                if clustered_internal::num_cells(&page) > 0 {
                    return Ok(root_pid);
                }
                let child = clustered_internal::child_at(&page, 0)?;
                storage.free_page(root_pid)?;
                root_pid = child;
            }
            other => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!(
                        "clustered collapse_root expected clustered page, found {other:?}"
                    ),
                });
            }
        }
    }
}
