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
pub mod schema_composite;
pub mod schema_exchange_rate;
pub mod schema_foreign_server;
pub mod schema_foreign_table;
pub mod schema_holiday_calendar;
pub mod schema_procedure;
pub mod writer;

pub use bootstrap::{CatalogBootstrap, CatalogPageIds};
pub use notifier::{
    CatalogChangeNotifier, SchemaChangeEvent, SchemaChangeKind, SchemaChangeListener,
};
pub use reader::CatalogReader;
pub use resolver::{ResolvedTable, SchemaResolver};
pub use schema::{
    AggregateDef, AggregateHelperKind, ColumnDef, ColumnType, ConstraintDef, ConstraintKind,
    ConstraintOperator, CronJobDef, DatabaseDef, EnumTypeDef, ExclusionElementDef, FkAction, FkDef,
    IdentityKind, IndexColumnDef, IndexDef, RelationKind, SchemaDef, SequenceDef, SortOrder,
    StatsDef, TableDatabaseDef, TableDef, TableId, TablePersistence, TableStorageLayout,
    TriggerDef, TriggerEvent, DEFAULT_DATABASE_NAME,
};
pub use schema_composite::{CompositeField, CompositeTypeDef};
pub use schema_exchange_rate::ExchangeRateDef;
pub use schema_foreign_server::ForeignServerDef;
pub use schema_foreign_table::{ForeignColumnDef, ForeignTableDef, FOREIGN_TABLE_ID_BASE};
pub use schema_holiday_calendar::HolidayCalendarDef;
pub use schema_procedure::{ProcLanguage, ProcParam, ProcParamMode, ProcedureDef};
pub use writer::{
    CatalogWriter, SYSTEM_TABLE_COLUMNS, SYSTEM_TABLE_COMPOSITE_TYPES, SYSTEM_TABLE_CONSTRAINTS,
    SYSTEM_TABLE_CRON_JOBS, SYSTEM_TABLE_DATABASES, SYSTEM_TABLE_ENUM_TYPES,
    SYSTEM_TABLE_EXCHANGE_RATES, SYSTEM_TABLE_FOREIGN_KEYS, SYSTEM_TABLE_FOREIGN_SERVERS,
    SYSTEM_TABLE_FOREIGN_TABLES, SYSTEM_TABLE_HOLIDAY_CALENDARS, SYSTEM_TABLE_INDEXES,
    SYSTEM_TABLE_SCHEMAS, SYSTEM_TABLE_SEQUENCES, SYSTEM_TABLE_STATS, SYSTEM_TABLE_TABLES,
    SYSTEM_TABLE_TABLE_DATABASES,
};
