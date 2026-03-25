//! Integration tests for fill factor (Phase 6.8).
//!
//! Tests cover DDL parsing, catalog storage, validation, the B-Tree split
//! threshold function, backward compatibility, and the behavioral invariant
//! that fillfactor=100 reproduces current behavior.

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute_with_ctx, parse, BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_wal::TxnManager;

// ── Test helper ───────────────────────────────────────────────────────────────

struct Db {
    storage: MemoryStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
    ctx: SessionContext,
    _dir: tempfile::TempDir,
}

impl Db {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let wal = dir.path().join("t.wal");
        let mut storage = MemoryStorage::new();
        CatalogBootstrap::init(&mut storage).unwrap();
        let txn = TxnManager::create(&wal).unwrap();
        Self {
            storage,
            txn,
            bloom: BloomRegistry::new(),
            ctx: SessionContext::new(),
            _dir: dir,
        }
    }

    fn run(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        let stmt = parse(sql, None)?;
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = analyze(stmt, &self.storage, snap)?;
        execute_with_ctx(
            analyzed,
            &mut self.storage,
            &mut self.txn,
            &mut self.bloom,
            &mut self.ctx,
        )
    }

    fn ok(&mut self, sql: &str) -> QueryResult {
        self.run(sql)
            .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e}"))
    }

    fn err(&mut self, sql: &str) -> DbError {
        self.run(sql)
            .expect_err(&format!("expected error for: {sql}"))
    }

    fn snapshot(&self) -> axiomdb_core::TransactionSnapshot {
        self.txn.snapshot()
    }
}

macro_rules! setup {
    ($db:expr, $($sql:expr),+) => { $( $db.ok($sql); )+ };
}

// ── fill_threshold unit tests ─────────────────────────────────────────────────

#[test]
fn test_fill_threshold_100_equals_order_leaf() {
    use axiomdb_index::page_layout::ORDER_LEAF;
    // fillfactor=100 must produce exactly ORDER_LEAF — regression guard.
    assert_eq!(
        axiomdb_index::BTree::fill_threshold_pub(ORDER_LEAF, 100),
        ORDER_LEAF,
        "fillfactor=100 must equal ORDER_LEAF (no behavioral regression)"
    );
}

#[test]
fn test_fill_threshold_90() {
    use axiomdb_index::page_layout::ORDER_LEAF;
    // ceil(217 * 90 / 100) = ceil(195.3) = 196
    assert_eq!(
        axiomdb_index::BTree::fill_threshold_pub(ORDER_LEAF, 90),
        196
    );
}

#[test]
fn test_fill_threshold_70() {
    use axiomdb_index::page_layout::ORDER_LEAF;
    // ceil(217 * 70 / 100) = ceil(151.9) = 152
    assert_eq!(
        axiomdb_index::BTree::fill_threshold_pub(ORDER_LEAF, 70),
        152
    );
}

#[test]
fn test_fill_threshold_10() {
    use axiomdb_index::page_layout::ORDER_LEAF;
    // ceil(217 * 10 / 100) = ceil(21.7) = 22
    assert_eq!(axiomdb_index::BTree::fill_threshold_pub(ORDER_LEAF, 10), 22);
}

#[test]
fn test_fill_threshold_never_zero() {
    // Even with order=1 and fillfactor=1, threshold is at least 1.
    assert!(axiomdb_index::BTree::fill_threshold_pub(1, 10) >= 1);
}

// ── DDL: parsing and catalog storage ─────────────────────────────────────────

#[test]
fn test_create_index_with_fillfactor_persists() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");
    db.ok("CREATE INDEX idx_x ON t(x) WITH (fillfactor = 70)");

    let snap = db.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let t = reader.get_table("public", "t").unwrap().unwrap();
    let indexes = reader.list_indexes(t.id).unwrap();
    let idx = indexes.iter().find(|i| i.name == "idx_x").unwrap();
    assert_eq!(idx.fillfactor, 70, "fillfactor=70 must be persisted");
}

#[test]
fn test_create_index_default_fillfactor_is_90() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");
    db.ok("CREATE INDEX idx_x ON t(x)");

    let snap = db.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let t = reader.get_table("public", "t").unwrap().unwrap();
    let indexes = reader.list_indexes(t.id).unwrap();
    let idx = indexes.iter().find(|i| i.name == "idx_x").unwrap();
    assert_eq!(idx.fillfactor, 90, "default fillfactor must be 90");
}

#[test]
fn test_create_unique_index_with_fillfactor() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE users (id INT PRIMARY KEY, email TEXT)");
    db.ok("CREATE UNIQUE INDEX uq_email ON users(email) WITH (fillfactor = 80)");

    let snap = db.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let t = reader.get_table("public", "users").unwrap().unwrap();
    let indexes = reader.list_indexes(t.id).unwrap();
    let idx = indexes.iter().find(|i| i.name == "uq_email").unwrap();
    assert_eq!(idx.fillfactor, 80);
    assert!(idx.is_unique);
}

#[test]
fn test_create_partial_index_with_fillfactor() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT, active INT)");
    db.ok("CREATE INDEX idx_active ON t(x) WHERE active = 1 WITH (fillfactor = 60)");

    let snap = db.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let t = reader.get_table("public", "t").unwrap().unwrap();
    let indexes = reader.list_indexes(t.id).unwrap();
    let idx = indexes.iter().find(|i| i.name == "idx_active").unwrap();
    assert_eq!(idx.fillfactor, 60, "fillfactor on partial index");
    assert!(idx.predicate.is_some(), "predicate must also be stored");
}

// ── Validation ────────────────────────────────────────────────────────────────

#[test]
fn test_fillfactor_below_10_rejected() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");
    let err = db.err("CREATE INDEX idx ON t(x) WITH (fillfactor = 9)");
    assert!(
        matches!(err, DbError::ParseError { .. }),
        "fillfactor=9 must be rejected: {err}"
    );
}

#[test]
fn test_fillfactor_above_100_rejected() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");
    let err = db.err("CREATE INDEX idx ON t(x) WITH (fillfactor = 101)");
    assert!(
        matches!(err, DbError::ParseError { .. }),
        "fillfactor=101 must be rejected: {err}"
    );
}

#[test]
fn test_fillfactor_exactly_10_accepted() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");
    db.ok("CREATE INDEX idx ON t(x) WITH (fillfactor = 10)");
}

#[test]
fn test_fillfactor_exactly_100_accepted() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");
    db.ok("CREATE INDEX idx ON t(x) WITH (fillfactor = 100)");
}

#[test]
fn test_unknown_with_option_rejected() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");
    let err = db.err("CREATE INDEX idx ON t(x) WITH (deadlockfactor = 50)");
    assert!(
        matches!(err, DbError::ParseError { .. }),
        "unknown option must be rejected: {err}"
    );
}

// ── Behavioral: fillfactor=100 is identical to current behavior ───────────────

#[test]
fn test_fillfactor_100_no_regression() {
    // With fillfactor=100, the index must behave identically to an index
    // without a fill factor: all inserts succeed, all lookups work.
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, x INT)",
        "CREATE UNIQUE INDEX idx ON t(x) WITH (fillfactor = 100)"
    );
    for i in 1..=100 {
        db.ok(&format!("INSERT INTO t VALUES ({i}, {i})"));
    }
    // Lookup via index
    let result = db.ok("SELECT x FROM t WHERE x = 42");
    match result {
        QueryResult::Rows { rows, .. } => {
            assert_eq!(rows.len(), 1);
        }
        _ => panic!("expected rows"),
    }
}

// ── Serde: backward-compat ───────────────────────────────────────────────────

#[test]
fn test_index_def_fillfactor_roundtrip() {
    use axiomdb_catalog::schema::{IndexColumnDef, IndexDef, SortOrder};

    let def = IndexDef {
        index_id: 5,
        table_id: 2,
        name: "idx_ts".to_string(),
        root_page_id: 77,
        is_unique: false,
        is_primary: false,
        columns: vec![IndexColumnDef {
            col_idx: 1,
            order: SortOrder::Asc,
        }],
        predicate: None,
        fillfactor: 70,
        is_fk_index: false,
    };

    let bytes = def.to_bytes();
    let (decoded, consumed) = IndexDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded.fillfactor, 70);
    assert_eq!(consumed, bytes.len());
}

#[test]
fn test_pre_68_index_row_reads_fillfactor_as_90() {
    use axiomdb_catalog::schema::{IndexColumnDef, IndexDef, SortOrder};

    // Simulate a pre-6.8 row: serialize a def that has no fillfactor byte.
    // Since to_bytes() always writes fillfactor, we truncate the result to
    // simulate the old on-disk format (predicate section included, fillfactor missing).
    let def = IndexDef {
        index_id: 1,
        table_id: 1,
        name: "old_idx".to_string(),
        root_page_id: 5,
        is_unique: false,
        is_primary: false,
        columns: vec![IndexColumnDef {
            col_idx: 0,
            order: SortOrder::Asc,
        }],
        predicate: None,
        fillfactor: 90,
        is_fk_index: false,
    };

    let full_bytes = def.to_bytes();
    // Drop the last byte (the fillfactor byte) to simulate a pre-6.8 row.
    let old_bytes = &full_bytes[..full_bytes.len() - 1];
    let (decoded, _) = IndexDef::from_bytes(old_bytes).unwrap();
    assert_eq!(
        decoded.fillfactor, 90,
        "pre-6.8 row must deserialize with fillfactor=90 (default)"
    );
}
