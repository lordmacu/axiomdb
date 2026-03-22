//! # nexusdb-storage — motor de almacenamiento: páginas, mmap, WAL, free list

pub mod freelist;
pub mod memory;
pub mod mmap;
pub mod page;

pub use freelist::FreeList;
pub use memory::MemoryStorage;
pub use mmap::MmapStorage;
pub use page::{Page, PageType, HEADER_SIZE, PAGE_MAGIC, PAGE_SIZE};
