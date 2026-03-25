//! # axiomdb-catalog — schema types, catalog bootstrap, reader, writer, and notifier
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
pub use schema::{
    ColumnDef, ColumnType, ConstraintDef, FkAction, FkDef, IndexColumnDef, IndexDef, SortOrder,
    StatsDef, TableDef, TableId,
};
pub use writer::{
    CatalogWriter, SYSTEM_TABLE_COLUMNS, SYSTEM_TABLE_CONSTRAINTS, SYSTEM_TABLE_FOREIGN_KEYS,
    SYSTEM_TABLE_INDEXES, SYSTEM_TABLE_STATS, SYSTEM_TABLE_TABLES,
};
