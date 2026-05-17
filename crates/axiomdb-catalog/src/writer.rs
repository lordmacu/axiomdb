//! CatalogWriter — DDL write operations over the three system catalog tables.
//!
//! ## Responsibilities
//!
//! - Insert rows into `axiom_tables`, `axiom_columns`, `axiom_indexes` heap pages.
//! - Delete rows (MVCC: stamps `txn_id_deleted`; rows remain for older snapshots).
//! - WAL-log every mutation via [`TxnManager`] for crash recovery.
//! - Allocate monotonically increasing `TableId` and `IndexId` from the meta page.
//!
//! ## Usage
//!
//! The caller is responsible for `begin()` / `commit()` / `rollback()` on the
//! `TxnManager`. `CatalogWriter` only calls `record_insert` / `record_delete`
//! which require an active transaction.
//!
//! ```rust,ignore
//! txn.begin()?;
//! let mut writer = CatalogWriter::new(&mut storage, &mut txn)?;
//! let table_id = writer.create_table("public", "users")?;
//! writer.create_column(ColumnDef { table_id, col_idx: 0, name: "id".into(),
//!     col_type: ColumnType::BigInt, nullable: false })?;
//! txn.commit()?;
//! ```
//!
//! ## WAL table_id convention for system tables
//!
//! User `TableId`s start at 1 and grow upward. System tables use the top of
//! the `u32` range to avoid collisions:
//!
//! ```text
//! SYSTEM_TABLE_TABLES  = u32::MAX - 2  (axiom_tables)
//! SYSTEM_TABLE_COLUMNS = u32::MAX - 1  (axiom_columns)
//! SYSTEM_TABLE_INDEXES = u32::MAX      (axiom_indexes)
//! ```

use std::collections::HashSet;
use std::sync::Arc;

use axiomdb_core::error::DbError;
use axiomdb_storage::{
    alloc_constraint_id, alloc_fk_id, alloc_index_id, alloc_table_id, clustered_leaf,
    write_meta_u64, HeapChain, Page, PageType, StorageEngine,
};
use axiomdb_wal::{ConnectionTxn, TxnManager};

use crate::{
    bootstrap::{CatalogBootstrap, CatalogPageIds},
    notifier::{CatalogChangeNotifier, SchemaChangeEvent, SchemaChangeKind},
    schema::{
        AggregateDef, ColumnDef, ConstraintDef, CronJobDef, DatabaseDef, EnumTypeDef, FkDef,
        IndexDef, RelationKind, SequenceDef, StatsDef, TableDatabaseDef, TableDef, TableId,
        TablePersistence, TableStorageLayout,
    },
    schema_composite::CompositeTypeDef,
    schema_exchange_rate::ExchangeRateDef,
    schema_foreign_server::ForeignServerDef,
    schema_foreign_table::ForeignTableDef,
    schema_holiday_calendar::HolidayCalendarDef,
};

// ── WAL table_id constants for system tables ──────────────────────────────────

/// WAL `table_id` used for inserts/deletes into `axiom_tables`.
pub const SYSTEM_TABLE_TABLES: u32 = u32::MAX - 2;
/// WAL `table_id` used for inserts/deletes into `axiom_columns`.
pub const SYSTEM_TABLE_COLUMNS: u32 = u32::MAX - 1;
/// WAL `table_id` used for inserts/deletes into `axiom_indexes`.
pub const SYSTEM_TABLE_INDEXES: u32 = u32::MAX;
/// WAL `table_id` used for inserts/deletes into `axiom_constraints`.
pub const SYSTEM_TABLE_CONSTRAINTS: u32 = u32::MAX - 3;
/// WAL `table_id` used for inserts/deletes into `axiom_foreign_keys` (Phase 6.5).
pub const SYSTEM_TABLE_FOREIGN_KEYS: u32 = u32::MAX - 4;
/// WAL `table_id` used for inserts/deletes into `axiom_stats` (Phase 6.10).
pub const SYSTEM_TABLE_STATS: u32 = u32::MAX - 5;
/// WAL `table_id` used for inserts/deletes into `axiom_databases` (Phase 22b.3a).
pub const SYSTEM_TABLE_DATABASES: u32 = u32::MAX - 6;
/// WAL `table_id` used for inserts/deletes into `axiom_table_databases` (Phase 22b.3a).
pub const SYSTEM_TABLE_TABLE_DATABASES: u32 = u32::MAX - 7;
/// WAL `table_id` used for inserts/deletes into `axiom_schemas` (Phase 22b.4).
pub const SYSTEM_TABLE_SCHEMAS: u32 = u32::MAX - 8;
/// WAL `table_id` used for inserts/deletes into `axiom_aggregates` (Phase 13.14).
pub const SYSTEM_TABLE_AGGREGATES: u32 = u32::MAX - 9;
/// WAL `table_id` used for inserts/deletes into `axiom_sequences` (Phase 20.2).
pub const SYSTEM_TABLE_SEQUENCES: u32 = u32::MAX - 10;
/// WAL `table_id` used for inserts/deletes into `axiom_enum_types` (Phase 20.3).
pub const SYSTEM_TABLE_ENUM_TYPES: u32 = u32::MAX - 11;
/// WAL `table_id` used for inserts/deletes into `axiom_cron_jobs` (Phase 22b.1).
pub const SYSTEM_TABLE_CRON_JOBS: u32 = u32::MAX - 12;
/// WAL `table_id` used for inserts/deletes into `axiom_foreign_servers` (Phase 22b.2).
pub const SYSTEM_TABLE_FOREIGN_SERVERS: u32 = u32::MAX - 13;
/// WAL `table_id` used for inserts/deletes into `axiom_foreign_tables` (Phase 22b.2).
pub const SYSTEM_TABLE_FOREIGN_TABLES: u32 = u32::MAX - 14;
/// WAL `table_id` used for inserts/deletes into `axiom_holiday_calendars` (Phase 20.16).
pub const SYSTEM_TABLE_HOLIDAY_CALENDARS: u32 = u32::MAX - 15;
/// WAL `table_id` used for inserts/deletes into `axiom_exchange_rates` (Phase 20.17).
pub const SYSTEM_TABLE_EXCHANGE_RATES: u32 = u32::MAX - 16;
/// WAL `table_id` used for inserts/deletes into `axiom_composite_types` (Phase 20.18).
pub const SYSTEM_TABLE_COMPOSITE_TYPES: u32 = u32::MAX - 17;

fn validate_enum_type_def(def: &EnumTypeDef) -> Result<(), DbError> {
    if def.labels.is_empty() {
        return Err(DbError::InvalidValue {
            reason: format!(
                "enum type '{}.{}' must have at least one label",
                def.schema_name, def.name
            ),
        });
    }
    let mut seen = HashSet::with_capacity(def.labels.len());
    for label in &def.labels {
        if !seen.insert(label) {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "duplicate enum label '{}' in type '{}.{}'",
                    label, def.schema_name, def.name
                ),
            });
        }
    }
    Ok(())
}

// ── CatalogWriter ─────────────────────────────────────────────────────────────

/// DDL write access to the three system catalog tables.
///
/// Requires an active transaction in the `TxnManager`. All heap mutations
/// are WAL-logged for crash recovery and MVCC correctness.
///
/// Optionally carries a [`CatalogChangeNotifier`] that receives a
/// [`SchemaChangeEvent`] after each successful DDL mutation. Set it via
/// [`with_notifier`]. Without a notifier the writer behaves identically to
/// before — the notifier is purely additive.
///
/// [`with_notifier`]: CatalogWriter::with_notifier
pub struct CatalogWriter<'a> {
    storage: &'a dyn StorageEngine,
    txn: &'a TxnManager,
    conn: &'a mut ConnectionTxn,
    page_ids: CatalogPageIds,
    notifier: Option<Arc<CatalogChangeNotifier>>,
}

impl<'a> CatalogWriter<'a> {
    /// Creates a new `CatalogWriter` without a notifier.
    ///
    /// # Errors
    /// - [`DbError::CatalogNotInitialized`] if [`CatalogBootstrap::init`] has not been called.
    pub fn new(
        storage: &'a dyn StorageEngine,
        txn: &'a TxnManager,
        conn: &'a mut ConnectionTxn,
    ) -> Result<Self, DbError> {
        let page_ids = CatalogBootstrap::ensure_database_roots(storage)?;
        Ok(Self {
            storage,
            txn,
            conn,
            page_ids,
            notifier: None,
        })
    }

    pub fn create_aggregate(&mut self, def: AggregateDef) -> Result<(), DbError> {
        let root = self.page_ids.aggregates;
        if root == 0 {
            return Err(DbError::Internal {
                message: "aggregate catalog root not initialized".into(),
            });
        }
        let data = def.to_bytes();
        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) = HeapChain::insert(self.storage, root, &data, txn_id, None)?;
        let key = format!("{}.{}", def.schema_name, def.name);
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_AGGREGATES,
            key.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    pub fn delete_aggregate(
        &mut self,
        schema: &str,
        name: &str,
        arg_count: usize,
    ) -> Result<bool, DbError> {
        let root = self.page_ids.aggregates;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = AggregateDef::from_bytes(&data)?;
            if def.schema_name == schema
                && def.name.eq_ignore_ascii_case(name)
                && def.arg_types.len() == arg_count
            {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = format!("{}.{}", def.schema_name, def.name);
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_AGGREGATES,
                    key.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn create_sequence(&mut self, def: SequenceDef) -> Result<(), DbError> {
        let root = self.page_ids.sequences;
        if root == 0 {
            return Err(DbError::Internal {
                message: "sequence catalog root not initialized".into(),
            });
        }
        let data = def.to_bytes();
        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) = HeapChain::insert(self.storage, root, &data, txn_id, None)?;
        let key = format!("{}.{}", def.schema_name, def.name);
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_SEQUENCES,
            key.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    pub fn replace_sequence_state(&mut self, def: SequenceDef) -> Result<(), DbError> {
        let root = self.page_ids.sequences;
        if root == 0 {
            return Err(DbError::Internal {
                message: "sequence catalog root not initialized".into(),
            });
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (old, _) = SequenceDef::from_bytes(&data)?;
            if old.schema_name == def.schema_name && old.name.eq_ignore_ascii_case(&def.name) {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = format!("{}.{}", old.schema_name, old.name);
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_SEQUENCES,
                    key.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                self.create_sequence(def)?;
                return Ok(());
            }
        }

        Err(DbError::InvalidValue {
            reason: format!("sequence '{}.{}' not found", def.schema_name, def.name),
        })
    }

    pub fn delete_sequence(&mut self, schema: &str, name: &str) -> Result<bool, DbError> {
        let root = self.page_ids.sequences;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = SequenceDef::from_bytes(&data)?;
            if def.schema_name == schema && def.name.eq_ignore_ascii_case(name) {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = format!("{}.{}", def.schema_name, def.name);
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_SEQUENCES,
                    key.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    pub fn create_enum_type(&mut self, def: EnumTypeDef) -> Result<(), DbError> {
        let root = self.page_ids.enum_types;
        if root == 0 {
            return Err(DbError::Internal {
                message: "enum type catalog root not initialized".into(),
            });
        }
        validate_enum_type_def(&def)?;
        let data = def.to_bytes();
        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) = HeapChain::insert(self.storage, root, &data, txn_id, None)?;
        let key = format!("{}.{}", def.schema_name, def.name);
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_ENUM_TYPES,
            key.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    pub fn delete_enum_type(&mut self, schema: &str, name: &str) -> Result<bool, DbError> {
        let root = self.page_ids.enum_types;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = EnumTypeDef::from_bytes(&data)?;
            if def.schema_name == schema && def.name.eq_ignore_ascii_case(name) {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = format!("{}.{}", def.schema_name, def.name);
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_ENUM_TYPES,
                    key.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ── Composite type operations (Phase 20.18) ──────────────────────────────

    pub fn create_composite_type(&mut self, def: CompositeTypeDef) -> Result<(), DbError> {
        let root = if self.page_ids.composite_types != 0 {
            self.page_ids.composite_types
        } else {
            let r = CatalogBootstrap::ensure_composite_types_root(self.storage)?;
            self.page_ids.composite_types = r;
            r
        };
        if def.fields.is_empty() {
            return Err(DbError::InvalidValue {
                reason: format!(
                    "composite type '{}.{}' must have at least one field",
                    def.schema_name, def.name
                ),
            });
        }
        let data = def.to_bytes();
        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) = HeapChain::insert(self.storage, root, &data, txn_id, None)?;
        let key = format!("{}.{}", def.schema_name, def.name);
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_COMPOSITE_TYPES,
            key.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    pub fn delete_composite_type(&mut self, schema: &str, name: &str) -> Result<bool, DbError> {
        let root = self.page_ids.composite_types;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;
        for (page_id, slot_id, data) in rows {
            let (def, _) = CompositeTypeDef::from_bytes(&data)?;
            if def.schema_name == schema && def.name.eq_ignore_ascii_case(name) {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = format!("{}.{}", def.schema_name, def.name);
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_COMPOSITE_TYPES,
                    key.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Attaches a [`CatalogChangeNotifier`].
    ///
    /// After this call, every DDL operation fires the appropriate
    /// [`SchemaChangeEvent`] on the notifier immediately after the heap
    /// mutation succeeds (before commit — see notifier module docs for
    /// firing semantics).
    ///
    /// Returns `self` for builder-style chaining:
    /// ```rust,ignore
    /// let writer = CatalogWriter::new(&mut storage, &mut txn)?
    ///     .with_notifier(Arc::clone(&notifier));
    /// ```
    pub fn with_notifier(mut self, notifier: Arc<CatalogChangeNotifier>) -> Self {
        self.notifier = Some(notifier);
        self
    }

    // ── Internal: fire notification ───────────────────────────────────────────

    /// Fires a schema change event on the notifier, if one is set.
    ///
    /// Called after every successful DDL mutation. `txn_id` is taken from the
    /// active transaction; falls back to 0 if somehow called outside one
    /// (should not happen — DDL methods verify active txn before calling this).
    fn fire(&self, kind: SchemaChangeKind) {
        if let Some(n) = &self.notifier {
            let txn_id = self.conn.txn_id;
            n.notify(&SchemaChangeEvent { kind, txn_id });
        }
    }

    // ── Database operations ──────────────────────────────────────────────────

    /// Inserts a database definition row into `axiom_databases`.
    pub fn create_database(
        &mut self,
        name: &str,
        default_collation: Option<String>,
    ) -> Result<(), DbError> {
        let data = DatabaseDef {
            name: name.to_string(),
            default_collation,
        }
        .to_bytes();
        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) =
            HeapChain::insert(self.storage, self.page_ids.databases, &data, txn_id, None)?;
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_DATABASES,
            name.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    /// Deletes a database definition row from `axiom_databases`.
    ///
    /// Returns `Ok(false)` when no visible row exists.
    pub fn drop_database(&mut self, name: &str) -> Result<bool, DbError> {
        let snap = self.txn.active_snapshot(self.conn);
        let txn_id = self.conn.txn_id;
        let rows = crate::reader::CatalogReader::scan_databases_root(
            self.storage,
            self.page_ids.databases,
            snap,
        )?;
        for (rid, data) in rows {
            let (def, _) = DatabaseDef::from_bytes(&data)?;
            if def.name == name {
                HeapChain::delete(self.storage, rid.page_id, rid.slot_id, txn_id)?;
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_DATABASES,
                    name.as_bytes(),
                    &data,
                    rid.page_id,
                    rid.slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Inserts or replaces the table → database ownership binding.
    pub fn bind_table_to_database(
        &mut self,
        table_id: TableId,
        database_name: &str,
    ) -> Result<(), DbError> {
        self.drop_table_database_binding(table_id)?;
        let data = TableDatabaseDef {
            table_id,
            database_name: database_name.to_string(),
        }
        .to_bytes();
        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) = HeapChain::insert(
            self.storage,
            self.page_ids.table_databases,
            &data,
            txn_id,
            None,
        )?;
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_TABLE_DATABASES,
            &table_id.to_le_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    /// Deletes the explicit table → database ownership binding, if present.
    pub fn drop_table_database_binding(&mut self, table_id: TableId) -> Result<(), DbError> {
        let snap = self.txn.active_snapshot(self.conn);
        let txn_id = self.conn.txn_id;
        let rows = crate::reader::CatalogReader::scan_table_databases_root(
            self.storage,
            self.page_ids.table_databases,
            snap,
        )?;
        for (rid, data) in rows {
            let (def, _) = TableDatabaseDef::from_bytes(&data)?;
            if def.table_id == table_id {
                HeapChain::delete(self.storage, rid.page_id, rid.slot_id, txn_id)?;
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_TABLE_DATABASES,
                    &table_id.to_le_bytes(),
                    &data,
                    rid.page_id,
                    rid.slot_id,
                )?;
                return Ok(());
            }
        }
        Ok(())
    }

    /// Deletes all explicit table ownership bindings that point at `database_name`.
    ///
    /// Returns the affected table ids.
    pub fn drop_table_database_bindings_for_database(
        &mut self,
        database_name: &str,
    ) -> Result<Vec<TableId>, DbError> {
        let snap = self.txn.active_snapshot(self.conn);
        let txn_id = self.conn.txn_id;
        let rows = crate::reader::CatalogReader::scan_table_databases_root(
            self.storage,
            self.page_ids.table_databases,
            snap,
        )?;
        let mut dropped = Vec::new();
        for (rid, data) in rows {
            let (def, _) = TableDatabaseDef::from_bytes(&data)?;
            if def.database_name == database_name {
                HeapChain::delete(self.storage, rid.page_id, rid.slot_id, txn_id)?;
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_TABLE_DATABASES,
                    &def.table_id.to_le_bytes(),
                    &data,
                    rid.page_id,
                    rid.slot_id,
                )?;
                dropped.push(def.table_id);
            }
        }
        Ok(dropped)
    }

    // ── Schema operations (Phase 22b.4) ─────────────────────────────────────

    /// Inserts a schema definition row into `axiom_schemas`.
    ///
    /// The schemas root is lazily initialized if needed (legacy databases).
    pub fn create_schema(&mut self, database: &str, schema: &str) -> Result<(), DbError> {
        let root = self.ensure_schemas_root()?;
        let data = crate::schema::SchemaDef {
            database_name: database.to_string(),
            name: schema.to_string(),
        }
        .to_bytes();
        let txn_id = self.conn.txn_id;
        let key = format!("{}\0{}", database, schema);
        let (page_id, slot_id) = HeapChain::insert(self.storage, root, &data, txn_id, None)?;
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_SCHEMAS,
            key.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    /// Deletes the schema row from `axiom_schemas`.
    ///
    /// Returns `true` if a row was found and deleted, `false` if the schema row
    /// did not exist (e.g. `public` was never explicitly created — the schema is
    /// still considered logically present but has no catalog row).
    /// Precondition: caller has already ensured no tables remain (RESTRICT path)
    /// or has already dropped all contained tables (CASCADE path).
    pub fn delete_schema(&mut self, database: &str, schema: &str) -> Result<bool, DbError> {
        let root = self.page_ids.schemas;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;
        for (page_id, slot_id, data) in rows {
            let (def, _) = crate::schema::SchemaDef::from_bytes(&data)?;
            if def.database_name == database && def.name == schema {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = format!("{}\0{}", database, schema);
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_SCHEMAS,
                    key.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Ensures the `axiom_schemas` root page exists, allocating it lazily for
    /// legacy databases created before Phase 22b.4.
    fn ensure_schemas_root(&mut self) -> Result<u64, DbError> {
        if self.page_ids.schemas != 0 {
            return Ok(self.page_ids.schemas);
        }
        let root = self.storage.alloc_page(PageType::Data)?;
        let page = Page::new(PageType::Data, root);
        self.storage.write_page(root, &page)?;
        write_meta_u64(
            self.storage,
            axiomdb_storage::CATALOG_SCHEMAS_ROOT_BODY_OFFSET,
            root,
        )?;
        self.page_ids.schemas = root;
        Ok(root)
    }

    // ── Table operations ──────────────────────────────────────────────────────

    /// Allocates a new `TableId`, initializes a table root page for user row
    /// data, and inserts a row into `axiom_tables`.
    ///
    /// The row is WAL-logged as an Insert entry with
    /// `table_id = SYSTEM_TABLE_TABLES` and key = `allocated_table_id` as LE bytes.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::CatalogNotInitialized`] if sequences have not been seeded.
    /// - [`DbError::SequenceOverflow`] if the table ID space is exhausted.
    pub fn create_table(&mut self, schema: &str, name: &str) -> Result<TableId, DbError> {
        Ok(self
            .create_table_with_layout(schema, name, TableStorageLayout::Heap)?
            .id)
    }

    /// Allocates a new table using the requested storage layout.
    pub fn create_table_with_layout(
        &mut self,
        schema: &str,
        name: &str,
        storage_layout: TableStorageLayout,
    ) -> Result<TableDef, DbError> {
        self.create_relation_with_options(
            schema,
            name,
            storage_layout,
            false,
            TablePersistence::Permanent,
            RelationKind::Table,
            None,
            None,
        )
    }

    /// Allocates a new table with full option control.
    ///
    /// `immutable = true` declares the table as append-only (Phase 13.9 — the
    /// executor rejects UPDATE/DELETE on it).
    pub fn create_table_with_options(
        &mut self,
        schema: &str,
        name: &str,
        storage_layout: TableStorageLayout,
        immutable: bool,
        persistence: TablePersistence,
        default_collation: Option<String>,
    ) -> Result<TableDef, DbError> {
        self.create_relation_with_options(
            schema,
            name,
            storage_layout,
            immutable,
            persistence,
            RelationKind::Table,
            None,
            default_collation,
        )
    }

    /// Allocates a new relation with full option control.
    #[allow(clippy::too_many_arguments)]
    pub fn create_relation_with_options(
        &mut self,
        schema: &str,
        name: &str,
        storage_layout: TableStorageLayout,
        immutable: bool,
        persistence: TablePersistence,
        relation_kind: RelationKind,
        defining_query: Option<String>,
        default_collation: Option<String>,
    ) -> Result<TableDef, DbError> {
        let table_id = alloc_table_id(self.storage)?;
        let root_page_id = self.allocate_table_root(storage_layout)?;

        let def = TableDef {
            id: table_id,
            root_page_id,
            storage_layout,
            schema_name: schema.to_string(),
            table_name: name.to_string(),
            schema_version: 1,
            immutable,
            persistence,
            relation_kind,
            defining_query,
            default_collation,
            triggers: vec![],
        };
        let data = def.to_bytes();

        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) =
            HeapChain::insert(self.storage, self.page_ids.tables, &data, txn_id, None)?;

        let key = table_id.to_le_bytes();
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_TABLES,
            &key,
            &data,
            page_id,
            slot_id,
        )?;

        self.fire(SchemaChangeKind::TableCreated { table_id });
        Ok(def)
    }

    /// Creates a regular (non-materialized) view catalog entry.
    ///
    /// Unlike `create_relation_with_options`, this does **not** allocate a root
    /// page — views have no physical storage (`root_page_id = 0`).
    pub fn create_view(
        &mut self,
        schema: &str,
        name: &str,
        defining_query: String,
    ) -> Result<TableDef, DbError> {
        let table_id = alloc_table_id(self.storage)?;
        let def = TableDef {
            id: table_id,
            root_page_id: 0,
            storage_layout: TableStorageLayout::Heap,
            schema_name: schema.to_string(),
            table_name: name.to_string(),
            schema_version: 1,
            immutable: false,
            persistence: TablePersistence::Permanent,
            relation_kind: RelationKind::View,
            defining_query: Some(defining_query),
            default_collation: None,
            triggers: vec![],
        };
        let data = def.to_bytes();
        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) =
            HeapChain::insert(self.storage, self.page_ids.tables, &data, txn_id, None)?;
        let key = table_id.to_le_bytes();
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_TABLES,
            &key,
            &data,
            page_id,
            slot_id,
        )?;
        self.fire(SchemaChangeKind::TableCreated { table_id });
        Ok(def)
    }

    /// Replaces the defining query of an existing view (for `CREATE OR REPLACE VIEW`).
    pub fn replace_view_query(
        &mut self,
        table_id: TableId,
        new_query: String,
    ) -> Result<(), DbError> {
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;
                let new_def = TableDef {
                    defining_query: Some(new_query),
                    ..def
                };
                let new_data = new_def.to_bytes();
                let (pg2, sl2) =
                    HeapChain::insert(self.storage, self.page_ids.tables, &new_data, txn_id, None)?;
                self.txn.record_insert(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &new_data,
                    pg2,
                    sl2,
                )?;
                return Ok(());
            }
        }
        Err(DbError::Internal {
            message: format!("replace_view_query: table_id={table_id} not found"),
        })
    }

    fn allocate_table_root(&mut self, storage_layout: TableStorageLayout) -> Result<u64, DbError> {
        let (page_type, clustered) = match storage_layout {
            TableStorageLayout::Heap => (PageType::Data, false),
            TableStorageLayout::Clustered => (PageType::ClusteredLeaf, true),
        };

        let root_page_id = self.storage.alloc_page(page_type)?;
        let mut root_page = Page::new(page_type, root_page_id);
        if clustered {
            clustered_leaf::init_clustered_leaf(&mut root_page);
            root_page.update_checksum();
        }
        self.storage.write_page(root_page_id, &root_page)?;
        Ok(root_page_id)
    }

    // ── Column operations ─────────────────────────────────────────────────────

    /// Inserts a column definition row into `axiom_columns`.
    ///
    /// The caller is responsible for setting `col_idx` to the correct 0-based
    /// position. No uniqueness check is performed here (enforced by the executor).
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    pub fn create_column(&mut self, def: ColumnDef) -> Result<(), DbError> {
        let data = def.to_bytes();

        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) =
            HeapChain::insert(self.storage, self.page_ids.columns, &data, txn_id, None)?;

        // Key: (table_id, col_idx) as 6 bytes LE for WAL lookup.
        let mut key = [0u8; 6];
        key[0..4].copy_from_slice(&def.table_id.to_le_bytes());
        key[4..6].copy_from_slice(&def.col_idx.to_le_bytes());
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_COLUMNS,
            &key,
            &data,
            page_id,
            slot_id,
        )?;

        Ok(())
    }

    // ── Index operations ──────────────────────────────────────────────────────

    /// Allocates a new `index_id` and inserts an index definition row into
    /// `axiom_indexes`.
    ///
    /// The `def.index_id` field is ignored — the writer allocates a fresh ID
    /// from the meta page sequence and stores it in the row.
    ///
    /// Returns the allocated `index_id`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::SequenceOverflow`] if the index ID space is exhausted.
    pub fn create_index(&mut self, def: IndexDef) -> Result<u32, DbError> {
        let index_id = alloc_index_id(self.storage)?;

        // Build the row with the allocated index_id.
        let row = IndexDef { index_id, ..def };
        let data = row.to_bytes();

        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) =
            HeapChain::insert(self.storage, self.page_ids.indexes, &data, txn_id, None)?;

        let key = index_id.to_le_bytes();
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_INDEXES,
            &key,
            &data,
            page_id,
            slot_id,
        )?;

        self.fire(SchemaChangeKind::IndexCreated {
            index_id,
            table_id: row.table_id,
        });
        Ok(index_id)
    }

    // ── Drop operations ───────────────────────────────────────────────────────

    /// Marks all rows for `table_id` as deleted in `axiom_tables`,
    /// `axiom_columns`, and `axiom_indexes`.
    ///
    /// Uses `active_snapshot()` to see the writer's own uncommitted inserts,
    /// so a table created and immediately dropped in the same transaction is
    /// handled correctly.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    pub fn delete_table(&mut self, table_id: TableId) -> Result<(), DbError> {
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);

        // Collect rows first (releases the immutable borrow on storage).
        let table_rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap.clone())?;
        let col_rows = HeapChain::scan_visible(self.storage, self.page_ids.columns, snap.clone())?;
        let idx_rows = HeapChain::scan_visible(self.storage, self.page_ids.indexes, snap)?;

        // Delete matching rows from axiom_tables.
        for (page_id, slot_id, data) in table_rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;
            }
        }

        // Delete matching columns from axiom_columns.
        for (page_id, slot_id, data) in col_rows {
            let (def, _) = ColumnDef::from_bytes(&data)?;
            if def.table_id == table_id {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_COLUMNS,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;
            }
        }

        // Delete matching indexes from axiom_indexes; collect dropped index_ids for events.
        let mut dropped_index_ids: Vec<u32> = Vec::new();
        for (page_id, slot_id, data) in idx_rows {
            let (def, _) = IndexDef::from_bytes(&data)?;
            if def.table_id == table_id {
                dropped_index_ids.push(def.index_id);
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_INDEXES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;
            }
        }

        // Fire notifications after all mutations succeed.
        self.fire(SchemaChangeKind::TableDropped { table_id });
        for index_id in dropped_index_ids {
            self.fire(SchemaChangeKind::IndexDropped { index_id, table_id });
        }

        // Remove any explicit database ownership binding for this table.
        self.drop_table_database_binding(table_id)?;

        Ok(())
    }

    /// Marks the column row with `(table_id, col_idx)` as deleted in `axiom_columns`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::Internal`] if the column row is not found (caller must validate first).
    pub fn delete_column(&mut self, table_id: TableId, col_idx: u16) -> Result<(), DbError> {
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.columns, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = ColumnDef::from_bytes(&data)?;
            if def.table_id == table_id && def.col_idx == col_idx {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let mut key = [0u8; 6];
                key[0..4].copy_from_slice(&table_id.to_le_bytes());
                key[4..6].copy_from_slice(&col_idx.to_le_bytes());
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_COLUMNS,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(());
            }
        }
        Err(DbError::Internal {
            message: format!("delete_column: col_idx={col_idx} not found for table_id={table_id}"),
        })
    }

    /// Renames a column by deleting the old catalog row and inserting a new one.
    ///
    /// All other fields (`col_type`, `nullable`, `auto_increment`, `col_idx`) are preserved.
    pub fn rename_column(
        &mut self,
        table_id: TableId,
        col_idx: u16,
        new_name: String,
    ) -> Result<(), DbError> {
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.columns, snap)?;

        // Find and remember the old ColumnDef.
        let old_def = rows
            .into_iter()
            .find_map(|(_, _, data)| {
                ColumnDef::from_bytes(&data).ok().and_then(|(def, _)| {
                    if def.table_id == table_id && def.col_idx == col_idx {
                        Some(def)
                    } else {
                        None
                    }
                })
            })
            .ok_or_else(|| DbError::Internal {
                message: format!(
                    "rename_column: col_idx={col_idx} not found for table_id={table_id}"
                ),
            })?;

        self.delete_column(table_id, col_idx)?;
        self.create_column(ColumnDef {
            name: new_name,
            ..old_def
        })?;
        Ok(())
    }

    /// Renames a table by replacing its `TableDef` row in the catalog.
    ///
    /// The `table_id`, `root_page_id`, and `storage_layout` are preserved.
    pub fn rename_table(
        &mut self,
        table_id: TableId,
        new_name: String,
        schema: &str,
    ) -> Result<(), DbError> {
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                // Delete old row.
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;

                // Insert new row with updated name.
                let new_def = TableDef {
                    table_name: new_name,
                    schema_name: schema.to_string(),
                    ..def
                };
                let new_data = new_def.to_bytes();
                let (pg2, sl2) =
                    HeapChain::insert(self.storage, self.page_ids.tables, &new_data, txn_id, None)?;
                self.txn.record_insert(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &new_data,
                    pg2,
                    sl2,
                )?;
                return Ok(());
            }
        }
        Err(DbError::Internal {
            message: format!("rename_table: table_id={table_id} not found"),
        })
    }

    /// Replaces the `root_page_id` of a table in `axiom_tables`.
    ///
    /// Used by the bulk-empty fast path (Phase 5.16) to rotate the heap root
    /// to a freshly-allocated empty page. All other `TableDef` fields are preserved.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::Internal`] if `table_id` is not found in `axiom_tables`.
    pub fn update_table_root(
        &mut self,
        table_id: TableId,
        new_root_page_id: u64,
    ) -> Result<(), DbError> {
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                // Delete old row.
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;

                // Insert new row with updated root_page_id + bumped
                // schema_version. The version bump tells any caller that
                // cached this TableDef (resolve_table_cached, plan cache) to
                // re-resolve, because cached `root_page_id` is now stale.
                let new_def = TableDef {
                    root_page_id: new_root_page_id,
                    schema_version: def.schema_version + 1,
                    ..def
                };
                let new_data = new_def.to_bytes();
                let (pg2, sl2) =
                    HeapChain::insert(self.storage, self.page_ids.tables, &new_data, txn_id, None)?;
                self.txn.record_insert(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &new_data,
                    pg2,
                    sl2,
                )?;
                return Ok(());
            }
        }
        Err(DbError::Internal {
            message: format!("update_table_root: table_id={table_id} not found in axiom_tables"),
        })
    }

    /// Updates the storage layout of a table (e.g., Heap → Clustered during REBUILD).
    pub fn update_storage_layout(
        &mut self,
        table_id: TableId,
        new_layout: TableStorageLayout,
    ) -> Result<(), DbError> {
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;

                let new_def = TableDef {
                    storage_layout: new_layout,
                    ..def
                };
                let new_data = new_def.to_bytes();
                let (pg2, sl2) =
                    HeapChain::insert(self.storage, self.page_ids.tables, &new_data, txn_id, None)?;
                self.txn.record_insert(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &new_data,
                    pg2,
                    sl2,
                )?;
                return Ok(());
            }
        }
        Err(DbError::Internal {
            message: format!(
                "update_storage_layout: table_id={table_id} not found in axiom_tables"
            ),
        })
    }

    /// Increments the per-table `schema_version` counter in `axiom_tables`.
    ///
    /// Called by every DDL operation that structurally modifies a specific table
    /// (CREATE INDEX, DROP INDEX, DROP TABLE, TRUNCATE TABLE) so that the plan
    /// cache can detect stale entries without clearing the entire cache.
    ///
    /// Returns the new version value. Mirrors PostgreSQL's per-relation
    /// invalidation tracking in `relcache`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::Internal`] if `table_id` is not found in `axiom_tables`.
    pub fn bump_table_schema_version(&mut self, table_id: TableId) -> Result<u64, DbError> {
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;

                let new_version = def.schema_version + 1;
                let new_def = TableDef {
                    schema_version: new_version,
                    ..def
                };
                let new_data = new_def.to_bytes();
                let (pg2, sl2) =
                    HeapChain::insert(self.storage, self.page_ids.tables, &new_data, txn_id, None)?;
                self.txn.record_insert(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &new_data,
                    pg2,
                    sl2,
                )?;
                return Ok(new_version);
            }
        }
        Err(DbError::Internal {
            message: format!(
                "bump_table_schema_version: table_id={table_id} not found in axiom_tables"
            ),
        })
    }

    /// Replaces the trigger list of a table in `axiom_tables`.
    pub fn update_table_triggers(
        &mut self,
        table_id: TableId,
        new_triggers: Vec<crate::schema::TriggerDef>,
    ) -> Result<(), DbError> {
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.tables, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = TableDef::from_bytes(&data)?;
            if def.id == table_id {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = table_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;

                let new_def = TableDef {
                    triggers: new_triggers,
                    ..def
                };
                let new_data = new_def.to_bytes();
                let (pg2, sl2) =
                    HeapChain::insert(self.storage, self.page_ids.tables, &new_data, txn_id, None)?;
                self.txn.record_insert(
                    self.conn,
                    SYSTEM_TABLE_TABLES,
                    &key,
                    &new_data,
                    pg2,
                    sl2,
                )?;
                return Ok(());
            }
        }
        Err(DbError::Internal {
            message: format!(
                "update_table_triggers: table_id={table_id} not found in axiom_tables"
            ),
        })
    }

    /// Marks the index row with `index_id` as deleted in `axiom_indexes`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::CatalogIndexNotFound`] if no visible index with that ID exists.
    pub fn delete_index(&mut self, index_id: u32) -> Result<(), DbError> {
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);

        let rows = HeapChain::scan_visible(self.storage, self.page_ids.indexes, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = IndexDef::from_bytes(&data)?;
            if def.index_id == index_id {
                let table_id = def.table_id;
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = index_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_INDEXES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;
                self.fire(SchemaChangeKind::IndexDropped { index_id, table_id });
                return Ok(());
            }
        }

        Err(DbError::CatalogIndexNotFound { index_id })
    }

    /// Updates the `root_page_id` of an existing index.
    ///
    /// Called after a B-Tree root split during DML — the old catalog row is
    /// deleted and a new one is inserted with the updated `root_page_id`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - [`DbError::CatalogIndexNotFound`] if no visible index with that ID exists.
    pub fn update_index_root(&mut self, index_id: u32, new_root: u64) -> Result<(), DbError> {
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.indexes, snap)?;
        for (_, _, data) in rows {
            let (def, _) = IndexDef::from_bytes(&data)?;
            if def.index_id == index_id {
                return self.replace_index_def(IndexDef {
                    root_page_id: new_root,
                    ..def
                });
            }
        }
        Err(DbError::CatalogIndexNotFound { index_id })
    }

    /// Replaces the visible row for `def.index_id` with the provided definition.
    ///
    /// Preserves `index_id` and rewrites the full catalog payload, which allows
    /// callers to update root page, column list, INCLUDE columns, or other
    /// metadata in one MVCC-safe operation.
    pub fn replace_index_def(&mut self, def: IndexDef) -> Result<(), DbError> {
        let index_id = def.index_id;
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);

        let rows = HeapChain::scan_visible(self.storage, self.page_ids.indexes, snap)?;
        for (page_id, slot_id, data) in rows {
            let (existing, _) = IndexDef::from_bytes(&data)?;
            if existing.index_id == index_id {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = index_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_INDEXES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;

                let new_data = def.to_bytes();
                let (new_page_id, new_slot_id) = HeapChain::insert(
                    self.storage,
                    self.page_ids.indexes,
                    &new_data,
                    txn_id,
                    None,
                )?;
                self.txn.record_insert(
                    self.conn,
                    SYSTEM_TABLE_INDEXES,
                    &key,
                    &new_data,
                    new_page_id,
                    new_slot_id,
                )?;
                // Bump the owning table's schema_version so any cached
                // ResolvedTable / plan that listed this index re-resolves.
                // Mirrors the bump in update_table_root for table-root rotation.
                self.bump_table_schema_version(def.table_id)?;
                return Ok(());
            }
        }

        Err(DbError::CatalogIndexNotFound { index_id })
    }

    /// Renames an index in the catalog (ALTER TABLE RENAME INDEX).
    ///
    /// Deletes the old row and inserts an updated one with the new name.
    ///
    /// # Errors
    /// - [`DbError::CatalogIndexNotFound`] if no index with `index_id` is visible.
    pub fn rename_index(&mut self, index_id: u32, new_name: String) -> Result<(), DbError> {
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, self.page_ids.indexes, snap)?;
        for (page_id, slot_id, data) in rows {
            let (def, _) = IndexDef::from_bytes(&data)?;
            if def.index_id == index_id {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = index_id.to_le_bytes();
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_INDEXES,
                    &key,
                    &data,
                    page_id,
                    slot_id,
                )?;
                let updated = IndexDef {
                    name: new_name,
                    ..def
                };
                let new_data = updated.to_bytes();
                let (new_page_id, new_slot_id) = HeapChain::insert(
                    self.storage,
                    self.page_ids.indexes,
                    &new_data,
                    txn_id,
                    None,
                )?;
                self.txn.record_insert(
                    self.conn,
                    SYSTEM_TABLE_INDEXES,
                    &key,
                    &new_data,
                    new_page_id,
                    new_slot_id,
                )?;
                return Ok(());
            }
        }
        Err(DbError::CatalogIndexNotFound { index_id })
    }

    // ── Constraint operations (Phase 4.22b) ───────────────────────────────────

    /// Allocates a new `constraint_id` and inserts a constraint definition row
    /// into `axiom_constraints`.
    ///
    /// Returns the allocated `constraint_id`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    pub fn create_constraint(&mut self, def: ConstraintDef) -> Result<u32, DbError> {
        let constraint_id = alloc_constraint_id(self.storage)?;
        let constraints_root = CatalogBootstrap::ensure_constraints_root(self.storage)?;

        let row = ConstraintDef {
            constraint_id,
            ..def
        };
        let data = row.to_bytes();

        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) =
            HeapChain::insert(self.storage, constraints_root, &data, txn_id, None)?;

        let key = constraint_id.to_le_bytes();
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_CONSTRAINTS,
            &key,
            &data,
            page_id,
            slot_id,
        )?;

        Ok(constraint_id)
    }

    /// MVCC-deletes the constraint row with `constraint_id` from `axiom_constraints`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    /// - Returns `Ok(())` silently if the constraint is not found (idempotent).
    pub fn drop_constraint(&mut self, constraint_id: u32) -> Result<(), DbError> {
        let constraints_root = CatalogBootstrap::ensure_constraints_root(self.storage)?;
        let snap = self.txn.active_snapshot(self.conn);
        let txn_id = self.conn.txn_id;

        let rids = crate::reader::CatalogReader::scan_constraints_root(
            self.storage,
            constraints_root,
            snap,
        )?;
        for (rid, data) in rids {
            if let Ok((def, _)) = ConstraintDef::from_bytes(&data) {
                if def.constraint_id == constraint_id {
                    let key = constraint_id.to_le_bytes();
                    axiomdb_storage::HeapChain::delete(
                        self.storage,
                        rid.page_id,
                        rid.slot_id,
                        txn_id,
                    )?;
                    self.txn.record_delete(
                        self.conn,
                        SYSTEM_TABLE_CONSTRAINTS,
                        &key,
                        &data,
                        rid.page_id,
                        rid.slot_id,
                    )?;
                    return Ok(());
                }
            }
        }
        Ok(()) // not found — idempotent
    }

    // ── FK operations (Phase 6.5) ─────────────────────────────────────────────

    /// Allocates a new `fk_id` and inserts a FK definition row into
    /// `axiom_foreign_keys`.
    ///
    /// Returns the allocated `fk_id`.
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    pub fn create_foreign_key(&mut self, def: FkDef) -> Result<u32, DbError> {
        let fk_id = alloc_fk_id(self.storage)?;
        let fk_root = CatalogBootstrap::ensure_fk_root(self.storage)?;

        let row = FkDef { fk_id, ..def };
        let data = row.to_bytes();

        let txn_id = self.conn.txn_id;
        let (page_id, slot_id) = HeapChain::insert(self.storage, fk_root, &data, txn_id, None)?;

        let key = fk_id.to_le_bytes();
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_FOREIGN_KEYS,
            &key,
            &data,
            page_id,
            slot_id,
        )?;

        Ok(fk_id)
    }

    /// MVCC-deletes the FK row with `fk_id` from `axiom_foreign_keys`.
    ///
    /// Returns `Ok(())` silently if the FK is not found (idempotent).
    ///
    /// # Errors
    /// - [`DbError::NoActiveTransaction`] if no transaction is active.
    pub fn drop_foreign_key(&mut self, fk_id: u32) -> Result<(), DbError> {
        let fk_root = match CatalogBootstrap::page_ids(self.storage) {
            Ok(ids) if ids.foreign_keys != 0 => ids.foreign_keys,
            _ => return Ok(()), // no FK table yet — nothing to drop
        };
        let snap = self.txn.active_snapshot(self.conn);
        let txn_id = self.conn.txn_id;

        let rows = crate::reader::CatalogReader::scan_fk_root(self.storage, fk_root, snap)?;
        for (rid, data) in rows {
            if let Ok((def, _)) = FkDef::from_bytes(&data) {
                if def.fk_id == fk_id {
                    let key = fk_id.to_le_bytes();
                    axiomdb_storage::HeapChain::delete(
                        self.storage,
                        rid.page_id,
                        rid.slot_id,
                        txn_id,
                    )?;
                    self.txn.record_delete(
                        self.conn,
                        SYSTEM_TABLE_FOREIGN_KEYS,
                        &key,
                        &data,
                        rid.page_id,
                        rid.slot_id,
                    )?;
                    return Ok(());
                }
            }
        }
        Ok(()) // not found — idempotent
    }

    /// Replaces the visible row for `def.fk_id` with the provided FK definition.
    ///
    /// Preserves `fk_id` and rewrites the full catalog payload so callers can
    /// adjust column positions or auto-index references after ALTER TABLE.
    pub fn replace_foreign_key(&mut self, def: FkDef) -> Result<(), DbError> {
        let fk_root = match CatalogBootstrap::page_ids(self.storage) {
            Ok(ids) if ids.foreign_keys != 0 => ids.foreign_keys,
            _ => {
                return Err(DbError::CatalogTableNotFound {
                    table_id: SYSTEM_TABLE_FOREIGN_KEYS,
                })
            }
        };
        let fk_id = def.fk_id;
        let snap = self.txn.active_snapshot(self.conn);
        let txn_id = self.conn.txn_id;

        let rows = crate::reader::CatalogReader::scan_fk_root(self.storage, fk_root, snap)?;
        for (rid, data) in rows {
            if let Ok((existing, _)) = FkDef::from_bytes(&data) {
                if existing.fk_id == fk_id {
                    let key = fk_id.to_le_bytes();
                    HeapChain::delete(self.storage, rid.page_id, rid.slot_id, txn_id)?;
                    self.txn.record_delete(
                        self.conn,
                        SYSTEM_TABLE_FOREIGN_KEYS,
                        &key,
                        &data,
                        rid.page_id,
                        rid.slot_id,
                    )?;

                    let new_data = def.to_bytes();
                    let (new_page_id, new_slot_id) =
                        HeapChain::insert(self.storage, fk_root, &new_data, txn_id, None)?;
                    self.txn.record_insert(
                        self.conn,
                        SYSTEM_TABLE_FOREIGN_KEYS,
                        &key,
                        &new_data,
                        new_page_id,
                        new_slot_id,
                    )?;
                    return Ok(());
                }
            }
        }

        Ok(())
    }

    // ── Statistics operations (Phase 6.10) ───────────────────────────────────

    /// Upserts per-column statistics into `axiom_stats`.
    ///
    /// If a row already exists for `(table_id, col_idx)`, it is MVCC-deleted
    /// and the new row is inserted. Both operations run within the same txn.
    ///
    /// Called at `CREATE INDEX` (bootstrap) and `ANALYZE` (refresh).
    /// Statistics writes are advisory — callers may ignore errors.
    pub fn upsert_stats(&mut self, def: StatsDef) -> Result<(), DbError> {
        let stats_root = CatalogBootstrap::ensure_stats_root(self.storage)?;
        let snap = self.txn.active_snapshot(self.conn);
        let txn_id = self.conn.txn_id;

        // MVCC-delete existing row for this (table_id, col_idx) if present.
        let existing =
            crate::reader::CatalogReader::scan_stats_root(self.storage, stats_root, snap)?;
        for (rid, old_data) in existing {
            if let Ok((old_def, _)) = StatsDef::from_bytes(&old_data) {
                if old_def.table_id == def.table_id && old_def.col_idx == def.col_idx {
                    let key = [
                        old_def.table_id.to_le_bytes(),
                        [old_def.col_idx as u8, (old_def.col_idx >> 8) as u8, 0, 0],
                    ]
                    .concat();
                    axiomdb_storage::HeapChain::delete(
                        self.storage,
                        rid.page_id,
                        rid.slot_id,
                        txn_id,
                    )?;
                    self.txn.record_delete(
                        self.conn,
                        SYSTEM_TABLE_STATS,
                        &key,
                        &old_data,
                        rid.page_id,
                        rid.slot_id,
                    )?;
                    break;
                }
            }
        }

        // Insert new stats row.
        let data = def.to_bytes();
        let key = [
            def.table_id.to_le_bytes(),
            [def.col_idx as u8, (def.col_idx >> 8) as u8, 0, 0],
        ]
        .concat();
        let (page_id, slot_id) = HeapChain::insert(self.storage, stats_root, &data, txn_id, None)?;
        self.txn
            .record_insert(self.conn, SYSTEM_TABLE_STATS, &key, &data, page_id, slot_id)?;
        Ok(())
    }

    /// Deletes all stats rows for `table_id`.
    pub fn delete_stats_for_table(&mut self, table_id: TableId) -> Result<(), DbError> {
        let stats_root = match CatalogBootstrap::page_ids(self.storage) {
            Ok(ids) if ids.stats != 0 => ids.stats,
            _ => return Ok(()),
        };
        let snap = self.txn.active_snapshot(self.conn);
        let txn_id = self.conn.txn_id;
        let existing =
            crate::reader::CatalogReader::scan_stats_root(self.storage, stats_root, snap)?;
        for (rid, old_data) in existing {
            if let Ok((old_def, _)) = StatsDef::from_bytes(&old_data) {
                if old_def.table_id == table_id {
                    let key = [
                        old_def.table_id.to_le_bytes(),
                        [old_def.col_idx as u8, (old_def.col_idx >> 8) as u8, 0, 0],
                    ]
                    .concat();
                    HeapChain::delete(self.storage, rid.page_id, rid.slot_id, txn_id)?;
                    self.txn.record_delete(
                        self.conn,
                        SYSTEM_TABLE_STATS,
                        &key,
                        &old_data,
                        rid.page_id,
                        rid.slot_id,
                    )?;
                }
            }
        }
        Ok(())
    }

    // ── Cron job operations (Phase 22b.1) ─────────────────────────────────────

    /// Inserts or replaces a cron job definition in `axiom_cron_jobs`.
    ///
    /// If a job with the same name already exists (case-insensitive), the old
    /// row is deleted before inserting the new one (upsert semantics).
    pub fn upsert_cron_job(&mut self, def: CronJobDef) -> Result<(), DbError> {
        let root = CatalogBootstrap::ensure_cron_jobs_root(self.storage)?;
        self.page_ids.cron_jobs = root;

        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        // Delete existing job with the same name.
        for (page_id, slot_id, data) in rows {
            let (existing, _) = CronJobDef::from_bytes(&data)?;
            if existing.name.eq_ignore_ascii_case(&def.name) {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_CRON_JOBS,
                    existing.name.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                break;
            }
        }

        let data = def.to_bytes();
        let (page_id, slot_id) = HeapChain::insert(self.storage, root, &data, txn_id, None)?;
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_CRON_JOBS,
            def.name.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    /// Deletes a cron job by name. Returns `true` if found and deleted.
    pub fn delete_cron_job(&mut self, name: &str) -> Result<bool, DbError> {
        let root = self.page_ids.cron_jobs;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = CronJobDef::from_bytes(&data)?;
            if def.name.eq_ignore_ascii_case(name) {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_CRON_JOBS,
                    def.name.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Updates runtime state for a cron job after execution (called by the scheduler).
    ///
    /// Performs an in-place delete+insert to update `next_run_ms`, `last_run_ms`,
    /// and `last_status` without changing schedule/command/enabled.
    pub fn update_cron_job_run(
        &mut self,
        name: &str,
        last_run_ms: i64,
        next_run_ms: i64,
        status: &str,
    ) -> Result<bool, DbError> {
        let root = self.page_ids.cron_jobs;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, old_data) in rows {
            let (mut def, _) = CronJobDef::from_bytes(&old_data)?;
            if def.name.eq_ignore_ascii_case(name) {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_CRON_JOBS,
                    def.name.as_bytes(),
                    &old_data,
                    page_id,
                    slot_id,
                )?;
                def.last_run_ms = last_run_ms;
                def.next_run_ms = next_run_ms;
                def.last_status = status.chars().take(255).collect();
                let new_data = def.to_bytes();
                let (np, ns) = HeapChain::insert(self.storage, root, &new_data, txn_id, None)?;
                self.txn.record_insert(
                    self.conn,
                    SYSTEM_TABLE_CRON_JOBS,
                    def.name.as_bytes(),
                    &new_data,
                    np,
                    ns,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ── Foreign server operations (Phase 22b.2) ───────────────────────────────

    /// Inserts or replaces a foreign server definition in `axiom_foreign_servers`.
    ///
    /// Upsert semantics: an existing server with the same name is replaced.
    pub fn upsert_foreign_server(&mut self, def: ForeignServerDef) -> Result<(), DbError> {
        let root = CatalogBootstrap::ensure_foreign_servers_root(self.storage)?;
        self.page_ids.foreign_servers = root;

        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (existing, _) = ForeignServerDef::from_bytes(&data)?;
            if existing.name.eq_ignore_ascii_case(&def.name) {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_FOREIGN_SERVERS,
                    existing.name.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                break;
            }
        }

        let data = def.to_bytes();
        let (page_id, slot_id) = HeapChain::insert(self.storage, root, &data, txn_id, None)?;
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_FOREIGN_SERVERS,
            def.name.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    /// Deletes a foreign server by name. Returns `true` if found and deleted.
    pub fn delete_foreign_server(&mut self, name: &str) -> Result<bool, DbError> {
        let root = self.page_ids.foreign_servers;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = ForeignServerDef::from_bytes(&data)?;
            if def.name.eq_ignore_ascii_case(name) {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_FOREIGN_SERVERS,
                    def.name.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ── Foreign table operations (Phase 22b.2) ────────────────────────────────

    /// Inserts a new foreign table definition in `axiom_foreign_tables`.
    ///
    /// `def.table_id` must already be set to a valid foreign table ID
    /// (`>= FOREIGN_TABLE_ID_BASE`) before calling this method.
    pub fn insert_foreign_table(&mut self, def: ForeignTableDef) -> Result<(), DbError> {
        let root = CatalogBootstrap::ensure_foreign_tables_root(self.storage)?;
        self.page_ids.foreign_tables = root;

        let txn_id = self.conn.txn_id;
        let data = def.to_bytes();
        let key = format!("{}.{}", def.schema_name, def.table_name);
        let (page_id, slot_id) = HeapChain::insert(self.storage, root, &data, txn_id, None)?;
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_FOREIGN_TABLES,
            key.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    /// Deletes a foreign table by schema + table name. Returns `true` if found.
    pub fn delete_foreign_table(
        &mut self,
        schema: &str,
        table_name: &str,
    ) -> Result<bool, DbError> {
        let root = self.page_ids.foreign_tables;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = ForeignTableDef::from_bytes(&data)?;
            if def.schema_name.eq_ignore_ascii_case(schema)
                && def.table_name.eq_ignore_ascii_case(table_name)
            {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = format!("{}.{}", def.schema_name, def.table_name);
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_FOREIGN_TABLES,
                    key.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ── Holiday calendar operations (Phase 20.16) ─────────────────────────────

    /// Inserts or replaces a holiday calendar in `axiom_holiday_calendars`.
    ///
    /// If a calendar for the same country code already exists (case-insensitive),
    /// the old row is deleted before inserting the new one (upsert semantics).
    pub fn upsert_holiday_calendar(&mut self, def: HolidayCalendarDef) -> Result<(), DbError> {
        let root = CatalogBootstrap::ensure_holiday_calendars_root(self.storage)?;
        self.page_ids.holiday_calendars = root;

        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (existing, _) = HolidayCalendarDef::from_bytes(&data)?;
            if existing
                .country_code
                .eq_ignore_ascii_case(&def.country_code)
            {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_HOLIDAY_CALENDARS,
                    existing.country_code.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                break;
            }
        }

        let data = def.to_bytes();
        let (page_id, slot_id) = HeapChain::insert(self.storage, root, &data, txn_id, None)?;
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_HOLIDAY_CALENDARS,
            def.country_code.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    /// Deletes a holiday calendar by country code (case-insensitive).
    ///
    /// Returns `true` if the calendar was found and deleted, `false` if not found.
    pub fn delete_holiday_calendar(&mut self, country: &str) -> Result<bool, DbError> {
        let root = self.page_ids.holiday_calendars;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = HolidayCalendarDef::from_bytes(&data)?;
            if def.country_code.eq_ignore_ascii_case(country) {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_HOLIDAY_CALENDARS,
                    def.country_code.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    // ── Exchange rate operations (Phase 20.17) ────────────────────────────────

    /// Inserts or replaces an exchange rate in `axiom_exchange_rates`.
    ///
    /// If a rate for the same (from, to) pair already exists (case-insensitive),
    /// the old row is deleted before inserting the new one (upsert semantics).
    pub fn upsert_exchange_rate(&mut self, def: ExchangeRateDef) -> Result<(), DbError> {
        let root = CatalogBootstrap::ensure_exchange_rates_root(self.storage)?;
        self.page_ids.exchange_rates = root;

        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (existing, _) = ExchangeRateDef::from_bytes(&data)?;
            if existing
                .from_currency
                .eq_ignore_ascii_case(&def.from_currency)
                && existing.to_currency.eq_ignore_ascii_case(&def.to_currency)
            {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = format!("{}->{}", existing.from_currency, existing.to_currency);
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_EXCHANGE_RATES,
                    key.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                break;
            }
        }

        let data = def.to_bytes();
        let key = format!("{}->{}", def.from_currency, def.to_currency);
        let (page_id, slot_id) = HeapChain::insert(self.storage, root, &data, txn_id, None)?;
        self.txn.record_insert(
            self.conn,
            SYSTEM_TABLE_EXCHANGE_RATES,
            key.as_bytes(),
            &data,
            page_id,
            slot_id,
        )?;
        Ok(())
    }

    /// Deletes an exchange rate by (from, to) pair (case-insensitive).
    ///
    /// Returns `true` if found and deleted, `false` if not found.
    pub fn delete_exchange_rate(&mut self, from: &str, to: &str) -> Result<bool, DbError> {
        let root = self.page_ids.exchange_rates;
        if root == 0 {
            return Ok(false);
        }
        let txn_id = self.conn.txn_id;
        let snap = self.txn.active_snapshot(self.conn);
        let rows = HeapChain::scan_visible(self.storage, root, snap)?;

        for (page_id, slot_id, data) in rows {
            let (def, _) = ExchangeRateDef::from_bytes(&data)?;
            if def.from_currency.eq_ignore_ascii_case(from)
                && def.to_currency.eq_ignore_ascii_case(to)
            {
                HeapChain::delete(self.storage, page_id, slot_id, txn_id)?;
                let key = format!("{}->{}", def.from_currency, def.to_currency);
                self.txn.record_delete(
                    self.conn,
                    SYSTEM_TABLE_EXCHANGE_RATES,
                    key.as_bytes(),
                    &data,
                    page_id,
                    slot_id,
                )?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}
