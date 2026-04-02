use axiomdb_core::error::DbError;
use axiomdb_storage::{
    clustered_internal, clustered_leaf, clustered_tree, MemoryStorage, PageType, RowHeader,
    StorageEngine,
};

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

fn clustered_page_type(page_type: u8) -> Result<PageType, DbError> {
    PageType::try_from(page_type)
}

fn leftmost_leaf_pid(storage: &dyn StorageEngine, mut pid: u64) -> Result<u64, DbError> {
    loop {
        let page = storage.read_page(pid)?;
        match clustered_page_type(page.header().page_type)? {
            PageType::ClusteredLeaf => return Ok(pid),
            PageType::ClusteredInternal => {
                pid = clustered_internal::child_at(&page, 0)?;
            }
            other => {
                return Err(DbError::BTreeCorrupted {
                    msg: format!("expected clustered tree page, found {other:?} at {pid}"),
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
        let page_type = clustered_page_type(page.header().page_type)?;
        assert_eq!(page_type, PageType::ClusteredLeaf);

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
fn clustered_insert_keeps_ten_thousand_rows_sorted_across_root_splits() {
    let mut storage: Box<dyn StorageEngine> = Box::new(MemoryStorage::new());
    let mut root = None;

    for key in 0u32..10_000 {
        let row_len = 96 + (key as usize % 5) * 211;
        root = Some(
            clustered_tree::insert(
                storage.as_mut(),
                root,
                &key.to_be_bytes(),
                &row_header((key % 19) as u64 + 1),
                &row_bytes(key, row_len),
            )
            .unwrap(),
        );
    }

    let root_pid = root.unwrap();
    let root_page = storage.read_page(root_pid).unwrap();
    assert_eq!(
        clustered_page_type(root_page.header().page_type).unwrap(),
        PageType::ClusteredInternal
    );

    let keys = collect_leaf_chain_keys(storage.as_ref(), root_pid).unwrap();
    assert_eq!(keys.len(), 10_000);
    for (idx, key) in keys.iter().enumerate() {
        let expected = (idx as u32).to_be_bytes();
        assert_eq!(key.as_slice(), expected.as_slice());
    }
}
