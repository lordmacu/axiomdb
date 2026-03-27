//! Integration tests for partial indexes (Phase 6.7).
//!
//! Covers DDL (CREATE / DROP), INSERT/UPDATE/DELETE maintenance,
//! uniqueness semantics, and planner predicate-implication check.

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute_with_ctx, parse, BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
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

    fn rows(&mut self, sql: &str) -> Vec<Vec<Value>> {
        match self.ok(sql) {
            QueryResult::Rows { rows, .. } => rows,
            other => panic!("expected rows, got {other:?}"),
        }
    }
}

macro_rules! setup {
    ($db:expr, $($sql:expr),+) => { $( $db.ok($sql); )+ };
}

// ── DDL ───────────────────────────────────────────────────────────────────────

#[test]
fn test_create_partial_index_persists_predicate() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)"
    );
    db.ok("CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL");

    let snap = db.txn.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let users = reader.get_table("public", "users").unwrap().unwrap();
    let indexes = reader.list_indexes(users.id).unwrap();
    let partial = indexes.iter().find(|i| i.name == "uq_email").unwrap();
    assert_eq!(partial.predicate.as_deref(), Some("deleted_at IS NULL"));
    assert!(partial.is_unique);
}

#[test]
fn test_create_full_index_no_predicate() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t (id INT PRIMARY KEY, x INT)");
    db.ok("CREATE INDEX idx_x ON t(x)");

    let snap = db.txn.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let t = reader.get_table("public", "t").unwrap().unwrap();
    let indexes = reader.list_indexes(t.id).unwrap();
    let idx = indexes.iter().find(|i| i.name == "idx_x").unwrap();
    assert!(idx.predicate.is_none(), "full index must have no predicate");
}

#[test]
fn test_drop_partial_index() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL"
    );
    db.ok("DROP INDEX uq_email ON users");

    let snap = db.txn.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let users = reader.get_table("public", "users").unwrap().unwrap();
    let indexes = reader.list_indexes(users.id).unwrap();
    assert!(
        !indexes.iter().any(|i| i.name == "uq_email"),
        "index should be dropped"
    );
}

#[test]
fn test_create_partial_index_builds_from_existing_rows() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "INSERT INTO users VALUES (1, 'a@x.com', NULL)", // satisfies predicate
        "INSERT INTO users VALUES (2, 'b@x.com', '2025-01-01')"  // doesn't satisfy
    );
    // Creating index AFTER rows exist — build-time filtering must work.
    db.ok("CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL");

    // Inserting same email as row 1 (active) should fail.
    let err = db.err("INSERT INTO users VALUES (3, 'a@x.com', NULL)");
    assert!(matches!(err, DbError::UniqueViolation { .. }), "{err}");

    // Inserting same email as row 2 (deleted) is fine (not in index).
    db.ok("INSERT INTO users VALUES (4, 'b@x.com', NULL)");
}

// ── INSERT — predicate filtering ──────────────────────────────────────────────

#[test]
fn test_insert_satisfying_predicate_is_indexed() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL",
        "INSERT INTO users VALUES (1, 'alice@x.com', NULL)"
    );
    // Second active user with same email → violation
    let err = db.err("INSERT INTO users VALUES (2, 'alice@x.com', NULL)");
    assert!(matches!(err, DbError::UniqueViolation { .. }), "{err}");
}

#[test]
fn test_insert_not_satisfying_predicate_not_indexed() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL"
    );
    // Two deleted users with the same email — predicate not satisfied → no violation.
    db.ok("INSERT INTO users VALUES (1, 'alice@x.com', '2024-01-01')");
    db.ok("INSERT INTO users VALUES (2, 'alice@x.com', '2025-06-01')");
}

#[test]
fn test_partial_unique_one_active_one_deleted_ok() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL"
    );
    db.ok("INSERT INTO users VALUES (1, 'alice@x.com', NULL)"); // active
    db.ok("INSERT INTO users VALUES (2, 'alice@x.com', '2025-01-01')"); // deleted — OK
}

#[test]
fn test_multi_row_insert_partial_unique_mixed_membership_ok() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL"
    );
    db.ok("INSERT INTO users VALUES \
         (1, 'alice@x.com', NULL), \
         (2, 'alice@x.com', '2025-01-01')");

    let rows = db.rows("SELECT id, email, deleted_at FROM users ORDER BY id");
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Int(1),
                Value::Text("alice@x.com".into()),
                Value::Null,
            ],
            vec![
                Value::Int(2),
                Value::Text("alice@x.com".into()),
                Value::Text("2025-01-01".into()),
            ],
        ]
    );
}

#[test]
fn test_null_indexed_col_not_indexed() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE t (id INT PRIMARY KEY, x INT, flag TEXT)",
        "CREATE UNIQUE INDEX uq_x ON t(x) WHERE flag IS NULL"
    );
    // NULL in the indexed column is not inserted regardless of predicate.
    db.ok("INSERT INTO t VALUES (1, NULL, NULL)");
    db.ok("INSERT INTO t VALUES (2, NULL, NULL)"); // NULL x — not indexed even if predicate satisfied
}

// ── DELETE maintenance ────────────────────────────────────────────────────────

#[test]
fn test_delete_row_satisfying_predicate_vacates_index() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL",
        "INSERT INTO users VALUES (1, 'alice@x.com', NULL)"
    );
    db.ok("DELETE FROM users WHERE id = 1");
    // Now the slot is free — another active alice should succeed.
    db.ok("INSERT INTO users VALUES (2, 'alice@x.com', NULL)");
}

#[test]
fn test_delete_row_not_satisfying_predicate_noop_on_index() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL",
        "INSERT INTO users VALUES (1, 'alice@x.com', NULL)",
        "INSERT INTO users VALUES (2, 'bob@x.com', '2025-01-01')"
    );
    // Delete deleted user — should not affect the index state for alice.
    db.ok("DELETE FROM users WHERE id = 2");

    // alice still in index — duplicate still rejected.
    let err = db.err("INSERT INTO users VALUES (3, 'alice@x.com', NULL)");
    assert!(matches!(err, DbError::UniqueViolation { .. }), "{err}");
}

// ── UPDATE maintenance ────────────────────────────────────────────────────────

#[test]
fn test_update_moves_row_out_of_predicate() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL",
        "INSERT INTO users VALUES (1, 'alice@x.com', NULL)"
    );
    // Soft-delete user 1.
    db.ok("UPDATE users SET deleted_at = '2025-01-01' WHERE id = 1");

    // alice is no longer in the partial index — new active alice is allowed.
    db.ok("INSERT INTO users VALUES (2, 'alice@x.com', NULL)");
}

#[test]
fn test_update_moves_row_into_predicate() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL",
        "INSERT INTO users VALUES (1, 'alice@x.com', '2025-01-01')" // deleted
    );
    // Restore user (clear deleted_at) — moves into predicate, gets indexed.
    db.ok("UPDATE users SET deleted_at = NULL WHERE id = 1");

    // Now alice is active again — another active alice must fail.
    let err = db.err("INSERT INTO users VALUES (2, 'alice@x.com', NULL)");
    assert!(matches!(err, DbError::UniqueViolation { .. }), "{err}");
}

// ── Planner ───────────────────────────────────────────────────────────────────

#[test]
fn test_planner_uses_partial_index_when_predicate_implied() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL",
        "INSERT INTO users VALUES (1, 'alice@x.com', NULL)",
        "INSERT INTO users VALUES (2, 'bob@x.com', '2025-01-01')"
    );
    // Query with both indexed col = literal AND the predicate → planner CAN use index.
    // Correctness check: must return only the active user.
    let result =
        db.rows("SELECT email FROM users WHERE email = 'alice@x.com' AND deleted_at IS NULL");
    assert_eq!(result.len(), 1);
    assert_eq!(result[0][0], Value::Text("alice@x.com".into()));
}

#[test]
fn test_planner_skips_partial_index_when_predicate_not_implied() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, email TEXT, deleted_at TEXT)",
        "CREATE UNIQUE INDEX uq_email ON users(email) WHERE deleted_at IS NULL",
        "INSERT INTO users VALUES (1, 'alice@x.com', NULL)",
        "INSERT INTO users VALUES (2, 'alice@x.com', '2025-01-01')"
    );
    // Query without the predicate → planner must use full scan, returning BOTH rows.
    let result = db.rows("SELECT id FROM users WHERE email = 'alice@x.com'");
    assert_eq!(
        result.len(),
        2,
        "full scan must return both active and deleted"
    );
}

// ── Catalog: backward-compat serde ───────────────────────────────────────────

#[test]
fn test_index_def_roundtrip_with_predicate() {
    use axiomdb_catalog::schema::{IndexColumnDef, IndexDef, SortOrder};

    let def = IndexDef {
        index_id: 7,
        table_id: 3,
        name: "uq_email".to_string(),
        root_page_id: 42,
        is_unique: true,
        is_primary: false,
        columns: vec![IndexColumnDef {
            col_idx: 1,
            order: SortOrder::Asc,
        }],
        predicate: Some("deleted_at IS NULL".to_string()),
        fillfactor: 90,
        is_fk_index: false,
        include_columns: vec![],
    };

    let bytes = def.to_bytes();
    let (decoded, consumed) = IndexDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, def);
    assert_eq!(consumed, bytes.len());
    assert_eq!(decoded.predicate.as_deref(), Some("deleted_at IS NULL"));
}

#[test]
fn test_index_def_roundtrip_no_predicate_backward_compat() {
    use axiomdb_catalog::schema::{IndexColumnDef, IndexDef, SortOrder};

    let def = IndexDef {
        index_id: 5,
        table_id: 2,
        name: "idx_x".to_string(),
        root_page_id: 10,
        is_unique: false,
        is_primary: false,
        columns: vec![IndexColumnDef {
            col_idx: 0,
            order: SortOrder::Asc,
        }],
        predicate: None,
        fillfactor: 90,
        is_fk_index: false,
        include_columns: vec![],
    };

    let bytes = def.to_bytes();
    let (decoded, consumed) = IndexDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, def);
    assert_eq!(consumed, bytes.len());
    assert!(decoded.predicate.is_none());
}

#[test]
fn test_pre_67_index_row_reads_predicate_as_none() {
    use axiomdb_catalog::schema::{IndexColumnDef, IndexDef, SortOrder};

    // Simulate a pre-6.7 row: serialize with predicate=None (no pred_len bytes),
    // then verify from_bytes returns predicate=None.
    let old_def = IndexDef {
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
        include_columns: vec![],
    };

    let old_bytes = old_def.to_bytes();
    // Truncate to simulate old format (no predicate section).
    // The old format ends at the columns section.
    // Actually `to_bytes` with predicate=None already writes no pred_len bytes,
    // so this IS the old format.
    let (decoded, _) = IndexDef::from_bytes(&old_bytes).unwrap();
    assert!(
        decoded.predicate.is_none(),
        "pre-6.7 row must deserialize with predicate=None"
    );
}
