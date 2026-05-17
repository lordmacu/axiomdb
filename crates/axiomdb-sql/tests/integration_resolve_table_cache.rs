//! Tests for the versioned `ResolvedTable` cache in `SessionContext`.
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-A.md`.
//!
//! Step 1 (this file at first): unit tests of the new
//! `SessionContext::get_table_if_version` accessor on a hand-built
//! `ResolvedTable`. Subsequent steps add integration tests that drive
//! the full SQL stack.

use axiomdb_catalog::resolver::ResolvedTable;
use axiomdb_catalog::schema::{
    RelationKind, TableDef, TablePersistence, TableStorageLayout, DEFAULT_DATABASE_NAME,
};
use axiomdb_sql::SessionContext;

fn fake_resolved_table(id: u32, name: &str, schema_version: u64) -> ResolvedTable {
    ResolvedTable {
        def: TableDef {
            id,
            root_page_id: 1,
            storage_layout: TableStorageLayout::Heap,
            schema_name: "public".into(),
            table_name: name.into(),
            schema_version,
            immutable: false,
            persistence: TablePersistence::Permanent,
            relation_kind: RelationKind::Table,
            defining_query: None,
            default_collation: None,
            triggers: vec![],
        },
        columns: vec![],
        indexes: vec![],
        constraints: vec![],
        foreign_keys: vec![],
    }
}

#[test]
fn get_table_if_version_returns_some_on_match() {
    let mut ctx = SessionContext::default();
    ctx.cache_table(
        DEFAULT_DATABASE_NAME,
        "public",
        "t",
        fake_resolved_table(1, "t", 7),
    );
    let r = ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 7);
    assert!(r.is_some(), "cache hit expected when version matches");
    assert_eq!(r.unwrap().def.id, 1);
}

#[test]
fn get_table_if_version_returns_none_on_version_mismatch() {
    let mut ctx = SessionContext::default();
    ctx.cache_table(
        DEFAULT_DATABASE_NAME,
        "public",
        "t",
        fake_resolved_table(1, "t", 7),
    );
    assert!(
        ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 8)
            .is_none(),
        "version 8 must miss when cached is 7"
    );
    assert!(
        ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 6)
            .is_none(),
        "version 6 must miss when cached is 7 (older version means stale)"
    );
}

#[test]
fn get_table_if_version_returns_none_on_miss() {
    let ctx = SessionContext::default();
    assert!(
        ctx.get_table_if_version(DEFAULT_DATABASE_NAME, "public", "t", 0)
            .is_none(),
        "miss expected when nothing was cached"
    );
}

// ── Step 2: end-to-end test that cache hits actually happen inside an
// explicit transaction (the regression we are closing) ──────────────────

mod harness {
    use axiomdb_catalog::bootstrap::CatalogBootstrap;
    use axiomdb_core::error::DbError;
    use axiomdb_sql::{
        analyze_cached, bloom::BloomRegistry, execute_with_ctx, parse_with_sql_mode,
        result::QueryResult, SchemaCache, SessionContext,
    };
    use axiomdb_storage::MemoryStorage;
    use axiomdb_wal::TxnManager;

    pub struct Harness {
        pub storage: MemoryStorage,
        pub txn: TxnManager,
        pub bloom: BloomRegistry,
        pub schema_cache: SchemaCache,
        pub session: SessionContext,
    }

    impl Harness {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let wal_path = dir.keep().join("test.wal");
            let storage = MemoryStorage::new();
            CatalogBootstrap::init(&storage).expect("bootstrap");
            let txn = TxnManager::create(&wal_path).expect("create txn");
            Self {
                storage,
                txn,
                bloom: BloomRegistry::new(),
                schema_cache: SchemaCache::new(),
                session: SessionContext::default(),
            }
        }

        pub fn run(&mut self, sql: &str) -> Result<QueryResult, DbError> {
            let stmt = parse_with_sql_mode(sql, None, self.session.sql_mode_flags())?;
            let snap = if let Some(ref ct) = self.session.conn_txn {
                self.txn.active_snapshot(ct)
            } else {
                self.txn.snapshot()
            };
            let analyzed = analyze_cached(stmt, &self.storage, snap, &mut self.schema_cache)?;
            execute_with_ctx(
                analyzed,
                &self.storage,
                &self.txn,
                &self.bloom,
                &mut self.session,
            )
        }
    }
}

#[test]
fn cached_inside_txn_after_first_insert() {
    // 100 INSERTs into the same table inside one BEGIN..COMMIT.
    // Before this change: cache was BYPASSED inside txn, so cached_count()
    // stayed at 0 the whole time.
    // After this change: the first INSERT populates the cache; the next 99
    // hit it (validated by schema_version). cached_count() == 1.
    let mut h = harness::Harness::new();
    h.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    h.run("BEGIN").unwrap();

    let before = h.session.cached_count();
    for i in 1..=100 {
        h.run(&format!("INSERT INTO t VALUES ({i}, 'a')")).unwrap();
    }
    let after = h.session.cached_count();

    h.run("COMMIT").unwrap();

    assert_eq!(before, 0, "no entry cached before the first INSERT");
    assert_eq!(
        after, 1,
        "exactly one ResolvedTable cached for table 't' after 100 INSERTs \
         (proves cache hits work inside explicit txn, not just outside)"
    );
}
