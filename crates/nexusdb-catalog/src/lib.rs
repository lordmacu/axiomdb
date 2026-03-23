//! # nexusdb-catalog — schema types and catalog bootstrap
//!
//! - 3.11: [`CatalogBootstrap`], [`CatalogPageIds`], schema types
//! - 3.12: CatalogReader/Writer (coming)

pub mod bootstrap;
pub mod schema;

pub use bootstrap::{CatalogBootstrap, CatalogPageIds};
pub use schema::{ColumnDef, ColumnType, IndexDef, TableDef, TableId};
