//! Clustered B-tree insert path built on top of clustered leaf/internal pages.
//!
//! This module is intentionally storage-first: it provides the first mutable
//! tree controller for clustered pages without yet replacing the heap+index
//! executor path.

use axiomdb_core::error::DbError;

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

enum InsertResult {
    Inserted,
    Split { sep_key: Vec<u8>, right_pid: u64 },
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{clustered_internal, clustered_leaf, MemoryStorage};

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
