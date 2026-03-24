//! Integration tests for foreign key constraints (Phase 6.5 / 6.6).

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
        let wal_path = dir.path().join("test.wal");
        let mut storage = MemoryStorage::new();
        CatalogBootstrap::init(&mut storage).unwrap();
        let txn = TxnManager::create(&wal_path).unwrap();
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
    ($db:expr, $($sql:expr),+) => {
        $( $db.ok($sql); )+
    };
}

fn rows(result: QueryResult) -> Vec<Vec<Value>> {
    match result {
        QueryResult::Rows { rows, .. } => rows,
        other => panic!("expected Rows, got {other:?}"),
    }
}

// ── DDL ───────────────────────────────────────────────────────────────────────

#[test]
fn test_create_table_persists_fk() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY, name TEXT NOT NULL)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))"
    );
    let snap = db.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let users = reader.get_table("public", "users").unwrap().unwrap();
    let orders = reader.get_table("public", "orders").unwrap().unwrap();
    let fks = reader.list_fk_constraints(orders.id).unwrap();
    assert_eq!(fks.len(), 1);
    assert_eq!(fks[0].parent_table_id, users.id);
}

#[test]
fn test_create_table_fk_no_auto_index_phase_65() {
    // Phase 6.5: FK auto-index creation is deferred (B-Tree can't handle
    // duplicate keys in non-unique indexes). FK enforcement uses full scan.
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))"
    );
    let snap = db.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let orders = reader.get_table("public", "orders").unwrap().unwrap();
    let fks = reader.list_fk_constraints(orders.id).unwrap();
    assert_eq!(fks.len(), 1, "FK should be persisted in catalog");
    assert_eq!(
        fks[0].fk_index_id, 0,
        "fk_index_id should be 0 (no auto-index in Phase 6.5)"
    );
}

#[test]
fn test_create_table_fk_reuses_existing_index() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT)"
    );
    setup!(db, "CREATE INDEX idx_user ON orders (user_id)");
    db.ok("ALTER TABLE orders ADD CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users(id)");
    let snap = db.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let orders = reader.get_table("public", "orders").unwrap().unwrap();
    let fks = reader.list_fk_constraints(orders.id).unwrap();
    assert_eq!(fks.len(), 1);
    assert_eq!(
        fks[0].fk_index_id, 0,
        "fk_index_id should be 0 when reusing existing index"
    );
}

#[test]
fn test_create_table_fk_parent_not_found_error() {
    let mut db = Db::new();
    let err =
        db.err("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES nonexistent(id))");
    assert!(
        matches!(err, DbError::TableNotFound { .. }),
        "expected TableNotFound, got: {err}"
    );
}

#[test]
fn test_create_table_fk_no_parent_index_error() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE users (id INT, name TEXT)");
    let err = db.err("CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))");
    assert!(
        matches!(err, DbError::ForeignKeyNoParentIndex { .. }),
        "expected ForeignKeyNoParentIndex, got: {err}"
    );
}

#[test]
fn test_alter_table_add_fk() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT)"
    );
    db.ok(
        "ALTER TABLE orders ADD CONSTRAINT fk_orders_user FOREIGN KEY (user_id) REFERENCES users(id)",
    );
    let snap = db.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let orders = reader.get_table("public", "orders").unwrap().unwrap();
    let fks = reader.list_fk_constraints(orders.id).unwrap();
    assert_eq!(fks.len(), 1);
    assert_eq!(fks[0].name, "fk_orders_user");
}

#[test]
fn test_alter_table_add_fk_existing_violation_error() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT)",
        "INSERT INTO orders VALUES (1, 999)"
    );
    let err = db.err(
        "ALTER TABLE orders ADD CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users(id)",
    );
    assert!(
        matches!(err, DbError::ForeignKeyViolation { .. }),
        "expected ForeignKeyViolation, got: {err}"
    );
    // FK should NOT have been persisted.
    let snap = db.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let orders = reader.get_table("public", "orders").unwrap().unwrap();
    let fks = reader.list_fk_constraints(orders.id).unwrap();
    assert_eq!(
        fks.len(),
        0,
        "FK must not be persisted after validation failure"
    );
}

#[test]
fn test_alter_table_drop_fk() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))"
    );
    let snap = db.snapshot();
    let mut reader = axiomdb_catalog::CatalogReader::new(&db.storage, snap).unwrap();
    let orders = reader.get_table("public", "orders").unwrap().unwrap();
    let fks = reader.list_fk_constraints(orders.id).unwrap();
    assert_eq!(fks.len(), 1, "FK should exist before drop");
    let fk_name = fks[0].name.clone();

    db.ok(&format!("ALTER TABLE orders DROP CONSTRAINT {fk_name}"));

    let snap2 = db.snapshot();
    let mut reader2 = axiomdb_catalog::CatalogReader::new(&db.storage, snap2).unwrap();
    let fks_after = reader2.list_fk_constraints(orders.id).unwrap();
    assert_eq!(
        fks_after.len(),
        0,
        "FK should be removed from catalog after DROP"
    );
}

// ── INSERT validation ─────────────────────────────────────────────────────────

#[test]
fn test_insert_valid_fk() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))",
        "INSERT INTO users VALUES (1)"
    );
    db.ok("INSERT INTO orders VALUES (10, 1)");
}

#[test]
fn test_insert_null_fk_passes() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))"
    );
    db.ok("INSERT INTO orders VALUES (10, NULL)");
}

#[test]
fn test_insert_invalid_fk_error() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))"
    );
    let err = db.err("INSERT INTO orders VALUES (10, 999)");
    assert!(
        matches!(err, DbError::ForeignKeyViolation { .. }),
        "expected ForeignKeyViolation, got: {err}"
    );
}

#[test]
fn test_insert_multiple_fks_both_checked() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE products (id INT PRIMARY KEY)",
        "CREATE TABLE orders (
            id INT PRIMARY KEY,
            user_id INT REFERENCES users(id),
            product_id INT REFERENCES products(id)
         )",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO products VALUES (100)"
    );
    db.ok("INSERT INTO orders VALUES (1, 1, 100)");
    let err = db.err("INSERT INTO orders VALUES (2, 1, 999)");
    assert!(matches!(err, DbError::ForeignKeyViolation { .. }), "{err}");
}

// ── UPDATE child ──────────────────────────────────────────────────────────────

#[test]
fn test_update_fk_column_valid() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO users VALUES (2)",
        "INSERT INTO orders VALUES (10, 1)"
    );
    db.ok("UPDATE orders SET user_id = 2 WHERE id = 10");
}

#[test]
fn test_update_fk_column_invalid_error() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO orders VALUES (10, 1)"
    );
    let err = db.err("UPDATE orders SET user_id = 999 WHERE id = 10");
    assert!(
        matches!(err, DbError::ForeignKeyViolation { .. }),
        "expected ForeignKeyViolation, got: {err}"
    );
}

#[test]
fn test_update_non_fk_column_no_check() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id), note TEXT)",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO orders VALUES (10, 1, 'first')"
    );
    db.ok("UPDATE orders SET note = 'second' WHERE id = 10");
}

// ── DELETE parent — RESTRICT ──────────────────────────────────────────────────

#[test]
fn test_delete_parent_restrict_error() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (
            id INT PRIMARY KEY,
            user_id INT REFERENCES users(id) ON DELETE RESTRICT
         )",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO orders VALUES (10, 1)"
    );
    let err = db.err("DELETE FROM users WHERE id = 1");
    assert!(
        matches!(err, DbError::ForeignKeyParentViolation { .. }),
        "expected ForeignKeyParentViolation, got: {err}"
    );
}

#[test]
fn test_delete_parent_no_children_ok() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO users VALUES (2)"
    );
    db.ok("DELETE FROM users WHERE id = 1");
}

#[test]
fn test_delete_parent_null_children_ok() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO orders VALUES (10, NULL)"
    );
    db.ok("DELETE FROM users WHERE id = 1");
}

// ── DELETE parent — CASCADE ───────────────────────────────────────────────────

#[test]
fn test_delete_parent_cascade_removes_children() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (
            id INT PRIMARY KEY,
            user_id INT REFERENCES users(id) ON DELETE CASCADE
         )",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO orders VALUES (10, 1)",
        "INSERT INTO orders VALUES (11, 1)"
    );
    db.ok("DELETE FROM users WHERE id = 1");
    let res = rows(db.ok("SELECT id FROM orders WHERE user_id = 1"));
    assert_eq!(res.len(), 0, "cascade should delete child rows");
}

#[test]
fn test_delete_parent_cascade_multi_level() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE a (id INT PRIMARY KEY)",
        "CREATE TABLE b (id INT PRIMARY KEY, a_id INT REFERENCES a(id) ON DELETE CASCADE)",
        "CREATE TABLE c (id INT PRIMARY KEY, b_id INT REFERENCES b(id) ON DELETE CASCADE)",
        "INSERT INTO a VALUES (1)",
        "INSERT INTO b VALUES (10, 1)",
        "INSERT INTO c VALUES (100, 10)"
    );
    db.ok("DELETE FROM a WHERE id = 1");
    assert_eq!(rows(db.ok("SELECT id FROM b")).len(), 0);
    assert_eq!(rows(db.ok("SELECT id FROM c")).len(), 0);
}

#[test]
fn test_delete_parent_cascade_depth_exceeded() {
    let mut db = Db::new();
    setup!(db, "CREATE TABLE t0 (id INT PRIMARY KEY)");
    for i in 1..=11usize {
        let prev = i - 1;
        db.ok(&format!(
            "CREATE TABLE t{i} (id INT PRIMARY KEY, parent_id INT REFERENCES t{prev}(id) ON DELETE CASCADE)"
        ));
    }
    // Each level references the previous level with the same key value (1).
    setup!(db, "INSERT INTO t0 VALUES (1)");
    for i in 1..=11usize {
        db.ok(&format!("INSERT INTO t{i} VALUES (1, 1)"));
    }
    let err = db.err("DELETE FROM t0 WHERE id = 1");
    assert!(
        matches!(err, DbError::ForeignKeyCascadeDepth { .. }),
        "expected ForeignKeyCascadeDepth, got: {err}"
    );
}

// ── DELETE parent — SET NULL ──────────────────────────────────────────────────

#[test]
fn test_delete_parent_set_null() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (
            id INT PRIMARY KEY,
            user_id INT REFERENCES users(id) ON DELETE SET NULL
         )",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO orders VALUES (10, 1)"
    );
    db.ok("DELETE FROM users WHERE id = 1");
    let res = rows(db.ok("SELECT user_id FROM orders WHERE id = 10"));
    assert_eq!(res.len(), 1);
    assert!(matches!(res[0][0], Value::Null), "user_id should be NULL");
}

#[test]
fn test_delete_parent_set_null_not_nullable_error() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (
            id INT PRIMARY KEY,
            user_id INT NOT NULL REFERENCES users(id) ON DELETE SET NULL
         )",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO orders VALUES (10, 1)"
    );
    let err = db.err("DELETE FROM users WHERE id = 1");
    assert!(
        matches!(err, DbError::ForeignKeySetNullNotNullable { .. }),
        "expected ForeignKeySetNullNotNullable, got: {err}"
    );
}

// ── UPDATE parent key ─────────────────────────────────────────────────────────

#[test]
fn test_update_parent_key_with_children_restrict_error() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))",
        "INSERT INTO users VALUES (1)",
        "INSERT INTO orders VALUES (10, 1)"
    );
    let err = db.err("UPDATE users SET id = 99 WHERE id = 1");
    assert!(
        matches!(err, DbError::ForeignKeyParentViolation { .. }),
        "expected ForeignKeyParentViolation, got: {err}"
    );
}

#[test]
fn test_update_parent_key_no_children_ok() {
    let mut db = Db::new();
    setup!(
        db,
        "CREATE TABLE users (id INT PRIMARY KEY)",
        "CREATE TABLE orders (id INT PRIMARY KEY, user_id INT REFERENCES users(id))",
        "INSERT INTO users VALUES (1)"
    );
    db.ok("UPDATE users SET id = 99 WHERE id = 1");
}

// ── Catalog unit tests ────────────────────────────────────────────────────────

#[test]
fn test_fk_def_roundtrip() {
    use axiomdb_catalog::schema::{FkAction, FkDef};

    let original = FkDef {
        fk_id: 42,
        child_table_id: 10,
        child_col_idx: 2,
        parent_table_id: 5,
        parent_col_idx: 0,
        on_delete: FkAction::Cascade,
        on_update: FkAction::Restrict,
        fk_index_id: 7,
        name: "fk_orders_user".to_string(),
    };

    let bytes = original.to_bytes();
    let (decoded, consumed) = FkDef::from_bytes(&bytes).unwrap();
    assert_eq!(decoded, original);
    assert_eq!(consumed, bytes.len());
}

#[test]
fn test_fk_action_encoding_roundtrip() {
    use axiomdb_catalog::schema::FkAction;

    for action in [
        FkAction::NoAction,
        FkAction::Restrict,
        FkAction::Cascade,
        FkAction::SetNull,
        FkAction::SetDefault,
    ] {
        let byte = action as u8;
        let decoded = FkAction::try_from(byte).unwrap();
        assert_eq!(decoded as u8, byte);
    }
}

#[test]
fn test_legacy_db_fk_root_zero_returns_empty() {
    use axiomdb_core::TransactionSnapshot;

    let mut storage = MemoryStorage::new();
    CatalogBootstrap::init(&mut storage).unwrap();
    // Simulate pre-6.5 DB by zeroing FK root.
    axiomdb_storage::write_meta_u64(
        &mut storage,
        axiomdb_storage::CATALOG_FOREIGN_KEYS_ROOT_BODY_OFFSET,
        0,
    )
    .unwrap();

    let snap = TransactionSnapshot {
        snapshot_id: 0,
        current_txn_id: 0,
    };
    let mut reader = axiomdb_catalog::CatalogReader::new(&storage, snap).unwrap();
    assert_eq!(reader.list_fk_constraints(1).unwrap().len(), 0);
    assert_eq!(reader.list_fk_constraints_referencing(1).unwrap().len(), 0);
}
