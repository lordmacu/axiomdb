//! # nexusdb-catalog — schema types, catalog bootstrap, reader, writer, and notifier
//!
//! - 3.11: [`CatalogBootstrap`], [`CatalogPageIds`], schema types
//! - 3.12: [`CatalogReader`], [`CatalogWriter`]
//! - 3.13: [`CatalogChangeNotifier`], [`SchemaChangeEvent`], [`SchemaChangeListener`]

pub mod bootstrap;
pub mod notifier;
pub mod reader;
pub mod resolver;
pub mod schema;
pub mod writer;

pub use bootstrap::{CatalogBootstrap, CatalogPageIds};
pub use notifier::{
    CatalogChangeNotifier, SchemaChangeEvent, SchemaChangeKind, SchemaChangeListener,
};
pub use reader::CatalogReader;
pub use resolver::{ResolvedTable, SchemaResolver};
pub use schema::{ColumnDef, ColumnType, IndexDef, TableDef, TableId};
pub use writer::{CatalogWriter, SYSTEM_TABLE_COLUMNS, SYSTEM_TABLE_INDEXES, SYSTEM_TABLE_TABLES};
