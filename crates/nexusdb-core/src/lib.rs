//! # nexusdb-core
//!
//! Tipos base, traits y errores compartidos por todos los crates de NexusDB.
//! Sin dependencias externas excepto `thiserror`.

pub mod error;
pub mod traits;
pub mod types;

pub use error::DbError;
pub type Result<T> = std::result::Result<T, DbError>;
