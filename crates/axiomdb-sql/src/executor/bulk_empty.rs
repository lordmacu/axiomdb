struct BulkEmptyPlan {
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
    let rids = HeapChain::scan_rids_visible(storage, table_def.data_root_page_id, snap)?;
    let visible_row_count = rids.len() as u64;

    // Collect old page IDs before allocating new ones (avoids any overlap).
    let mut old_pages = collect_heap_chain_pages(storage, table_def.data_root_page_id)?;
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
    // Rotate heap root in the catalog.
    CatalogWriter::new(storage, txn)?.update_table_data_root(table_def.id, plan.new_data_root)?;

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
