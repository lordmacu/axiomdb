//! # axiomdb-index — B+ Tree CoW, iterators and prefix compression
//!
//! Implements a persistent B+ Tree over the [`StorageEngine`] trait:
//! - Variable binary keys up to 64 bytes
//! - Copy-on-Write with atomic root (`AtomicU64`)
//! - Lazy range scan via tree traversal (CoW invalidates `next_leaf`; O(log n) per leaf boundary)
//! - In-memory prefix compression for internal nodes

pub mod page_layout;
pub mod prefix;

mod iter;
mod tree;

pub use iter::RangeIter;
pub use tree::{fill_threshold, BTree};
