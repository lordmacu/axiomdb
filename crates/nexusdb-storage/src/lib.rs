//! # nexusdb-storage — motor de almacenamiento: páginas, mmap, WAL, free list

pub mod page;

pub use page::{Page, PageType, HEADER_SIZE, PAGE_MAGIC, PAGE_SIZE};
