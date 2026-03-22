//! # nexusdb-index — B+ Tree CoW, iteradores y prefix compression
//!
//! Implementa un B+ Tree persistente sobre el trait [`StorageEngine`]:
//! - Keys binarios variables hasta 64 bytes
//! - Copy-on-Write con raíz atómica (`AtomicU64`)
//! - Range scan lazy por linked list de hojas
//! - Prefix compression en memoria para nodos internos

pub mod page_layout;
pub mod prefix;

mod iter;
mod tree;

pub use iter::RangeIter;
pub use tree::BTree;
