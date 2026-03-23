//! # axiomdb-core
//!
//! Core types, traits, and errors shared by all AxiomDB crates.
//! No external dependencies except `thiserror`.

pub mod error;
pub mod traits;
pub mod types;

pub use error::DbError;
pub use traits::{Index, PageId, RecordId, TransactionSnapshot, TxnId};
pub type Result<T> = std::result::Result<T, DbError>;
