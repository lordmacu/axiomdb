//! # nexusdb-catalog — schema types, catalog bootstrap, reader, and writer
//!
//! - 3.11: [`CatalogBootstrap`], [`CatalogPageIds`], schema types
//! - 3.12: [`CatalogReader`], [`CatalogWriter`]

pub mod bootstrap;
pub mod reader;
pub mod schema;
pub mod writer;

pub use bootstrap::{CatalogBootstrap, CatalogPageIds};
pub use reader::CatalogReader;
pub use schema::{ColumnDef, ColumnType, IndexDef, TableDef, TableId};
pub use writer::{CatalogWriter, SYSTEM_TABLE_COLUMNS, SYSTEM_TABLE_INDEXES, SYSTEM_TABLE_TABLES};
