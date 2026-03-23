//! # nexusdb-storage — storage engine: pages, mmap, free list, heap, meta, integrity

pub mod config;
pub mod dirty;
pub mod engine;
pub mod freelist;
pub mod heap;
pub mod heap_chain;
pub mod integrity;
pub mod memory;
pub mod meta;
pub mod mmap;
pub mod page;

pub use config::DbConfig;
pub use dirty::PageDirtyTracker;
pub use engine::StorageEngine;
pub use freelist::FreeList;
pub use heap::{
    clear_deletion, delete_tuple, free_space, insert_tuple, mark_slot_dead, read_tuple,
    scan_visible, update_tuple, RowHeader, SlotEntry, MAX_TUPLE_DATA, MIN_TUPLE_OVERHEAD,
};
pub use heap_chain::{chain_next_page, chain_set_next_page, HeapChain};
pub use integrity::{IntegrityChecker, IntegrityReport, IntegrityViolation, Severity};
pub use memory::MemoryStorage;
pub use meta::{
    alloc_index_id, alloc_table_id, read_checkpoint_lsn, read_meta_u32, read_meta_u64,
    write_catalog_header, write_checkpoint_lsn, write_meta_u32, write_meta_u64,
    CATALOG_COLUMNS_ROOT_BODY_OFFSET, CATALOG_INDEXES_ROOT_BODY_OFFSET,
    CATALOG_SCHEMA_VER_BODY_OFFSET, CATALOG_TABLES_ROOT_BODY_OFFSET, CHECKPOINT_LSN_BODY_OFFSET,
    NEXT_INDEX_ID_BODY_OFFSET, NEXT_TABLE_ID_BODY_OFFSET,
};
pub use mmap::MmapStorage;
pub use page::{Page, PageType, HEADER_SIZE, PAGE_MAGIC, PAGE_SIZE};
