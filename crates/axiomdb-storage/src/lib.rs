//! # axiomdb-storage — storage engine: pages, mmap, free list, heap, meta, integrity

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
pub mod page_ref;

pub use config::{DbConfig, WalDurabilityPolicy};
pub use dirty::PageDirtyTracker;
pub use engine::StorageEngine;
pub use freelist::FreeList;
pub use heap::{
    clear_deletion, delete_tuple, free_space, insert_tuple, mark_slot_dead, num_slots, read_slot,
    read_tuple, read_tuple_header, read_tuple_image, restore_tuple_image, rewrite_tuple_same_slot,
    scan_visible, slot_capacity, update_tuple, RowHeader, SlotEntry, MAX_TUPLE_DATA,
    MIN_TUPLE_OVERHEAD,
};
pub use heap_chain::{chain_next_page, chain_set_next_page, HeapAppendHint, HeapChain};
pub use integrity::{IntegrityChecker, IntegrityReport, IntegrityViolation, Severity};
pub use memory::MemoryStorage;
pub use meta::{
    alloc_constraint_id, alloc_fk_id, alloc_index_id, alloc_table_id, read_checkpoint_lsn,
    read_meta_u32, read_meta_u64, write_catalog_header, write_checkpoint_lsn, write_meta_u32,
    write_meta_u64, CATALOG_COLUMNS_ROOT_BODY_OFFSET, CATALOG_CONSTRAINTS_ROOT_BODY_OFFSET,
    CATALOG_DATABASES_ROOT_BODY_OFFSET, CATALOG_FOREIGN_KEYS_ROOT_BODY_OFFSET,
    CATALOG_INDEXES_ROOT_BODY_OFFSET, CATALOG_SCHEMAS_ROOT_BODY_OFFSET,
    CATALOG_SCHEMA_VER_BODY_OFFSET, CATALOG_STATS_ROOT_BODY_OFFSET,
    CATALOG_TABLES_ROOT_BODY_OFFSET, CATALOG_TABLE_DATABASES_ROOT_BODY_OFFSET,
    CHECKPOINT_LSN_BODY_OFFSET, NEXT_CONSTRAINT_ID_BODY_OFFSET, NEXT_FK_ID_BODY_OFFSET,
    NEXT_INDEX_ID_BODY_OFFSET, NEXT_TABLE_ID_BODY_OFFSET,
};
pub use mmap::MmapStorage;
pub use page::{Page, PageType, HEADER_SIZE, PAGE_FLAG_ALL_VISIBLE, PAGE_MAGIC, PAGE_SIZE};
pub use page_ref::PageRef;
