pub(crate) struct BulkEmptyPlan {
    /// Rows visible to the statement snapshot — used as the DELETE row count.
    visible_row_count: u64,
    /// Freshly-allocated empty root page for the heap chain.
    new_data_root: u64,
    /// Freshly-allocated empty roots per index: `(index_id, new_root_page_id)`.
    new_index_roots: Vec<(u32, u64)>,
    /// All old pages to free AFTER commit durability is confirmed.
    old_pages_to_free: Vec<u64>,
}

/// Allocates a fresh empty heap-chain root page and returns its page_id.
fn alloc_empty_heap_root(storage: &mut dyn StorageEngine) -> Result<u64, DbError> {
    let pid = storage.alloc_page(PageType::Data)?;
    let page = Page::new(PageType::Data, pid);
    storage.write_page(pid, &page)?;
    Ok(pid)
}

/// Allocates a fresh empty B-Tree leaf root page and returns its page_id.
fn alloc_empty_index_root(storage: &mut dyn StorageEngine) -> Result<u64, DbError> {
    use axiomdb_index::page_layout::{cast_leaf_mut, NULL_PAGE};
    let pid = storage.alloc_page(PageType::Index)?;
    let mut page = Page::new(PageType::Index, pid);
    {
        let leaf = cast_leaf_mut(&mut page);
        leaf.is_leaf = 1;
        leaf.set_num_keys(0);
        leaf.set_next_leaf(NULL_PAGE);
    }
    page.update_checksum();
    storage.write_page(pid, &page)?;
    Ok(pid)
}

/// Collects all page_ids in a heap chain rooted at `root_page_id`.
///
/// Follows `chain_next_page(...)` links until `0`. The root page is included.
fn collect_heap_chain_pages(
    storage: &mut dyn StorageEngine,
    root_page_id: u64,
) -> Result<Vec<u64>, DbError> {
    let mut pages = Vec::new();
    let mut pid = root_page_id;
    while pid != 0 {
        pages.push(pid);
        let page = storage.read_page(pid)?;
        pid = chain_next_page(&page);
    }
    Ok(pages)
}

/// Collects all page_ids in a B-Tree rooted at `root_pid` (BFS walk).
///
/// The result includes internal nodes and leaf nodes but excludes `0` sentinels.
pub(crate) fn collect_btree_pages(
    storage: &mut dyn StorageEngine,
    root_pid: u64,
) -> Result<Vec<u64>, DbError> {
    use axiomdb_index::page_layout::cast_internal;

    let mut collected = Vec::new();
    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        collected.push(pid);
        let page = storage.read_page(pid)?;
        if page.body()[0] != 1 {
            // Internal node — push all children.
            let node = cast_internal(&page);
            let n = node.num_keys();
            for i in 0..=n {
                stack.push(node.child_at(i));
            }
        }
    }
    Ok(collected)
}

/// Collects all page IDs in an overflow chain starting at `first_pid`.
///
/// The first 8 bytes of each overflow page body encode the next page ID as
/// u64 LE; `u64::MAX` signals the end of the chain.
fn collect_overflow_chain(
    storage: &mut dyn StorageEngine,
    first_pid: u64,
) -> Result<Vec<u64>, DbError> {
    const NULL_PAGE: u64 = u64::MAX;
    let mut pages = Vec::new();
    let mut pid = first_pid;
    while pid != NULL_PAGE && pid != 0 {
        pages.push(pid);
        let page = storage.read_page(pid)?;
        let body = page.body();
        if body.len() < 8 {
            break;
        }
        pid = u64::from_le_bytes(body[..8].try_into().unwrap_or([0u8; 8]));
    }
    Ok(pages)
}

/// Collects all page IDs in a clustered B-tree rooted at `root_pid`, including
/// overflow chain pages attached to leaf cells.
pub(crate) fn collect_clustered_tree_pages(
    storage: &mut dyn StorageEngine,
    root_pid: u64,
) -> Result<Vec<u64>, DbError> {
    use axiomdb_storage::PageType;

    let mut collected = Vec::new();
    let mut stack = vec![root_pid];
    let mut visited = std::collections::HashSet::new();

    while let Some(pid) = stack.pop() {
        if !visited.insert(pid) {
            continue;
        }
        collected.push(pid);

        let page = storage.read_page(pid)?;
        let page_type = PageType::try_from(page.header().page_type)
            .unwrap_or(PageType::Data);

        match page_type {
            PageType::ClusteredLeaf => {
                let n = axiomdb_storage::clustered_leaf::num_cells(&page);
                for idx in 0..n {
                    if let Ok(cell) = axiomdb_storage::clustered_leaf::read_cell(&page, idx) {
                        if let Some(first_overflow) = cell.overflow_first_page {
                            collected.extend(collect_overflow_chain(storage, first_overflow)?);
                        }
                    }
                }
            }
            PageType::ClusteredInternal => {
                let n = axiomdb_storage::clustered_internal::num_cells(&page);
                for idx in 0..=n {
                    if let Ok(child) = axiomdb_storage::clustered_internal::child_at(&page, idx) {
                        if child != axiomdb_storage::clustered_internal::NULL_PAGE {
                            stack.push(child);
                        }
                    }
                }
            }
            _ => {} // unexpected page type — skip
        }
    }

    Ok(collected)
}

/// Plans a bulk-empty of a clustered table: counts visible rows, allocates a
/// fresh clustered root + fresh secondary index roots, and collects old page
/// IDs for deferred reclamation.
pub(crate) fn plan_bulk_empty_clustered_table(
    storage: &mut dyn StorageEngine,
    table_def: &axiomdb_catalog::TableDef,
    indexes: &[axiomdb_catalog::IndexDef],
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<BulkEmptyPlan, DbError> {
    // Count visible clustered rows for DELETE row-count semantics.
    let visible_row_count =
        crate::table::count_clustered_visible(storage, table_def.root_page_id, snap)?;

    // Collect old clustered pages BEFORE allocating new ones (avoids overlap).
    let mut old_pages = collect_clustered_tree_pages(storage, table_def.root_page_id)?;
    for idx in indexes {
        old_pages.extend(collect_btree_pages(storage, idx.root_page_id)?);
    }
    old_pages.sort_unstable();
    old_pages.dedup();

    // Allocate fresh empty clustered root and secondary index roots.
    let new_data_root = alloc_empty_clustered_root(storage)?;
    let mut new_index_roots = Vec::with_capacity(indexes.len());
    for idx in indexes {
        new_index_roots.push((idx.index_id, alloc_empty_index_root(storage)?));
    }

    Ok(BulkEmptyPlan {
        visible_row_count,
        new_data_root,
        new_index_roots,
        old_pages_to_free: old_pages,
    })
}

/// Plans a bulk table-empty operation: counts visible rows, allocates fresh roots,
/// and collects old page IDs for deferred reclamation.
///
/// Collect old pages FIRST, then allocate new ones so freshly-allocated pages
/// are never accidentally added to the free list.
fn plan_bulk_empty_table(
    storage: &mut dyn StorageEngine,
    table_def: &axiomdb_catalog::TableDef,
    indexes: &[axiomdb_catalog::IndexDef],
    snap: axiomdb_core::TransactionSnapshot,
) -> Result<BulkEmptyPlan, DbError> {
    // Count rows visible to this statement for DELETE row-count semantics.
    let rids = HeapChain::scan_rids_visible(storage, table_def.root_page_id, snap)?;
    let visible_row_count = rids.len() as u64;

    // Collect old page IDs before allocating new ones (avoids any overlap).
    let mut old_pages = collect_heap_chain_pages(storage, table_def.root_page_id)?;
    for idx in indexes {
        old_pages.extend(collect_btree_pages(storage, idx.root_page_id)?);
    }
    old_pages.sort_unstable();
    old_pages.dedup();

    // Allocate fresh empty roots AFTER collecting old IDs.
    let new_data_root = alloc_empty_heap_root(storage)?;
    let mut new_index_roots = Vec::with_capacity(indexes.len());
    for idx in indexes {
        new_index_roots.push((idx.index_id, alloc_empty_index_root(storage)?));
    }

    Ok(BulkEmptyPlan {
        visible_row_count,
        new_data_root,
        new_index_roots,
        old_pages_to_free: old_pages,
    })
}

/// Applies a [`BulkEmptyPlan`]: rotates heap + index roots in the catalog,
/// resets Bloom filters, schedules old pages for deferred free, and invalidates
/// the session schema cache.
///
/// All catalog mutations happen inside the current active transaction, so they
/// are fully undone on rollback or savepoint rollback.
fn apply_bulk_empty_table(
    storage: &mut dyn StorageEngine,
    txn: &mut TxnManager,
    bloom: &mut crate::bloom::BloomRegistry,
    table_def: &axiomdb_catalog::TableDef,
    plan: BulkEmptyPlan,
) -> Result<(), DbError> {
    // Rotate data root in the catalog (works for both heap and clustered tables).
    CatalogWriter::new(storage, txn)?.update_table_root(table_def.id, plan.new_data_root)?;

    // Rotate each index root in the catalog + reset its Bloom filter.
    for (index_id, new_root) in &plan.new_index_roots {
        CatalogWriter::new(storage, txn)?.update_index_root(*index_id, *new_root)?;
        // Reset bloom filter so old key presence checks return false.
        bloom.create(*index_id, 0);
    }

    // Enqueue old pages for post-commit reclamation.
    txn.defer_free_pages(plan.old_pages_to_free)?;

    Ok(())
}

/// Frees all pages of a B-Tree rooted at `root_pid`.
///
/// Iteratively walks the tree (BFS via a stack) and calls `free_page` on each
/// node — both internal and leaf pages.
pub(crate) fn free_btree_pages(
    storage: &mut dyn StorageEngine,
    root_pid: u64,
) -> Result<(), DbError> {
    use axiomdb_index::page_layout::{cast_internal, cast_leaf};

    let mut stack = vec![root_pid];
    while let Some(pid) = stack.pop() {
        let page = storage.read_page(pid)?;
        if page.body()[0] != 1 {
            // Internal node — push all children before freeing.
            let node = cast_internal(&page);
            let n = node.num_keys();
            for i in 0..=n {
                stack.push(node.child_at(i));
            }
        } else {
            // Leaf node — no children to push.
            let _leaf = cast_leaf(&page); // just validate it reads correctly
        }
        storage.free_page(pid)?;
    }
    Ok(())
}
