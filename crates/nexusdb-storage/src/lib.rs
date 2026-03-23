//! # nexusdb-storage — storage engine: pages, mmap, free list, heap

pub mod engine;
pub mod freelist;
pub mod heap;
pub mod memory;
pub mod mmap;
pub mod page;

pub use engine::StorageEngine;
pub use freelist::FreeList;
pub use heap::{
    clear_deletion, delete_tuple, free_space, insert_tuple, mark_slot_dead, read_tuple,
    scan_visible, update_tuple, RowHeader, SlotEntry, MAX_TUPLE_DATA, MIN_TUPLE_OVERHEAD,
};
pub use memory::MemoryStorage;
pub use mmap::MmapStorage;
pub use page::{Page, PageType, HEADER_SIZE, PAGE_MAGIC, PAGE_SIZE};
