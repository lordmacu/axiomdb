//! Integration tests for Phase 4 G9 — DDL completeness:
//! SHOW CREATE TABLE, RENAME TABLE, ALTER TABLE RENAME INDEX,
//! ALTER TABLE ADD INDEX / DROP INDEX, ALTER TABLE CHANGE COLUMN,
//! ALTER TABLE CONVERT TO CHARACTER SET, ALTER TABLE AUTO_INCREMENT=N,
//! SHOW WARNINGS / SHOW ERRORS.

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{analyze, execute_with_ctx, parse, BloomRegistry, QueryResult, SessionContext};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

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
        let storage = MemoryStorage::new();
        CatalogBootstrap::init(&storage).unwrap();
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
        let snap = if let Some(ref ct) = self.ctx.conn_txn {
            self.txn.active_snapshot(ct)
        } else {
            self.txn.snapshot()
        };
        let analyzed = analyze(stmt, &self.storage, snap)?;
        execute_with_ctx(
            analyzed,
            &self.storage,
            &self.txn,
            &self.bloom,
            &mut self.ctx,
        )
    }

    fn exec(&mut self, sql: &str) {
        self.run(sql)
            .unwrap_or_else(|e| panic!("SQL failed: {sql}\nError: {e:?}"));
    }

    fn rows(&mut self, sql: &str) -> Vec<Vec<Value>> {
        match self.run(sql).unwrap() {
            QueryResult::Rows { rows, .. } => rows,
            other => panic!("expected Rows, got {other:?}"),
        }
    }
}

// ── SHOW CREATE TABLE ─────────────────────────────────────────────────────────

#[test]
fn test_show_create_table_basic() {
    let mut db = Db::new();
    db.exec("CREATE TABLE users (id INT NOT NULL, name TEXT, score INT)");
    let rows = db.rows("SHOW CREATE TABLE users");
    assert_eq!(rows.len(), 1);
    let ddl = match &rows[0][1] {
        Value::Text(s) => s.clone(),
        other => panic!("expected Text, got {other:?}"),
    };
    assert!(
        ddl.contains("CREATE TABLE"),
        "DDL must start with CREATE TABLE: {ddl}"
    );
    assert!(ddl.contains("`id`"), "DDL must contain column 'id': {ddl}");
    assert!(
        ddl.contains("`name`"),
        "DDL must contain column 'name': {ddl}"
    );
    assert!(ddl.contains("NOT NULL"), "DDL must contain NOT NULL: {ddl}");
}

#[test]
fn test_show_create_table_with_pk() {
    let mut db = Db::new();
    db.exec("CREATE TABLE products (id INT NOT NULL, price INT, PRIMARY KEY (id))");
    let rows = db.rows("SHOW CREATE TABLE products");
    let ddl = match &rows[0][1] {
        Value::Text(s) => s.clone(),
        other => panic!("expected Text, got {other:?}"),
    };
    assert!(
        ddl.contains("PRIMARY KEY"),
        "DDL must include PRIMARY KEY: {ddl}"
    );
    assert!(ddl.contains("`id`"), "DDL must include PK column: {ddl}");
}

#[test]
fn test_show_create_table_not_found() {
    let mut db = Db::new();
    let err = db.run("SHOW CREATE TABLE nonexistent").unwrap_err();
    assert!(matches!(err, DbError::TableNotFound { .. }));
}

// ── RENAME TABLE ──────────────────────────────────────────────────────────────

#[test]
fn test_rename_table_basic() {
    let mut db = Db::new();
    db.exec("CREATE TABLE old_name (id INT)");
    db.exec("INSERT INTO old_name VALUES (42)");
    db.exec("RENAME TABLE old_name TO new_name");
    // Old name should be gone
    let err = db.run("SELECT * FROM old_name").unwrap_err();
    assert!(matches!(err, DbError::TableNotFound { .. }));
    // New name should work
    let rows = db.rows("SELECT * FROM new_name");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(42));
}

#[test]
fn test_rename_table_duplicate_fails() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t1 (id INT)");
    db.exec("CREATE TABLE t2 (id INT)");
    let err = db.run("RENAME TABLE t1 TO t2").unwrap_err();
    assert!(matches!(err, DbError::TableAlreadyExists { .. }));
}

// ── ALTER TABLE RENAME INDEX ──────────────────────────────────────────────────

#[test]
fn test_alter_table_rename_index() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t (id INT, val INT)");
    db.exec("CREATE INDEX idx_val ON t (val)");
    db.exec("ALTER TABLE t RENAME INDEX idx_val TO idx_value");
    // After rename, SHOW INDEX should reflect new name
    let rows = db.rows("SHOW INDEX FROM t");
    let names: Vec<_> = rows
        .iter()
        .map(|r| match &r[2] {
            Value::Text(s) => s.clone(),
            _ => String::new(),
        })
        .collect();
    assert!(
        names.contains(&"idx_value".to_string()),
        "Expected idx_value in index names: {names:?}"
    );
    assert!(
        !names.contains(&"idx_val".to_string()),
        "Old index name should be gone: {names:?}"
    );
}

// ── ALTER TABLE ADD INDEX / DROP INDEX ────────────────────────────────────────

#[test]
fn test_alter_table_add_index() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t (id INT, val INT)");
    db.exec("INSERT INTO t VALUES (1, 100), (2, 200)");
    db.exec("ALTER TABLE t ADD INDEX idx_val (val)");
    let rows = db.rows("SHOW INDEX FROM t");
    let names: Vec<_> = rows
        .iter()
        .map(|r| match &r[2] {
            Value::Text(s) => s.clone(),
            _ => String::new(),
        })
        .collect();
    assert!(
        names.contains(&"idx_val".to_string()),
        "Expected idx_val: {names:?}"
    );
}

#[test]
fn test_alter_table_add_unique_index() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t (id INT, email TEXT)");
    db.exec("ALTER TABLE t ADD UNIQUE INDEX idx_email (email)");
    let rows = db.rows("SHOW INDEX FROM t");
    let found = rows.iter().any(|r| {
        let name = match &r[2] {
            Value::Text(s) => s.as_str(),
            _ => "",
        };
        let non_unique = match &r[1] {
            Value::Int(n) => *n == 0,
            _ => false,
        };
        name == "idx_email" && non_unique
    });
    assert!(found, "Expected unique idx_email: {rows:?}");
}

#[test]
fn test_alter_table_drop_index() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t (id INT, val INT)");
    db.exec("CREATE INDEX idx_val ON t (val)");
    db.exec("ALTER TABLE t DROP INDEX idx_val");
    let rows = db.rows("SHOW INDEX FROM t");
    let names: Vec<_> = rows
        .iter()
        .map(|r| match &r[2] {
            Value::Text(s) => s.clone(),
            _ => String::new(),
        })
        .collect();
    assert!(
        !names.contains(&"idx_val".to_string()),
        "idx_val should be dropped: {names:?}"
    );
}

// ── ALTER TABLE CHANGE COLUMN ─────────────────────────────────────────────────

#[test]
fn test_alter_table_change_column_retype() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t (id INT, score INT)");
    db.exec("INSERT INTO t VALUES (1, 100)");
    // CHANGE COLUMN with same name (retype only)
    db.exec("ALTER TABLE t CHANGE COLUMN score score BIGINT");
    let rows = db.rows("SHOW COLUMNS FROM t");
    let score_row = rows
        .iter()
        .find(|r| r[0] == Value::Text("score".to_string()));
    assert!(score_row.is_some(), "score column not found after CHANGE");
    let type_val = match &score_row.unwrap()[1] {
        Value::Text(s) => s.clone(),
        _ => String::new(),
    };
    assert_eq!(type_val, "BIGINT");
}

#[test]
fn test_alter_table_change_column_rename() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t (id INT, old_col INT)");
    db.exec("INSERT INTO t VALUES (1, 42)");
    db.exec("ALTER TABLE t CHANGE COLUMN old_col new_col INT");
    // Verify new name works
    let rows = db.rows("SELECT new_col FROM t");
    assert_eq!(rows[0][0], Value::Int(42));
}

// ── ALTER TABLE CONVERT TO CHARACTER SET (no-op) ──────────────────────────────

#[test]
fn test_alter_table_convert_charset_noop() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t (id INT, name TEXT)");
    db.exec("INSERT INTO t VALUES (1, 'hello')");
    // Should be accepted without error
    db.exec("ALTER TABLE t CONVERT TO CHARACTER SET utf8mb4 COLLATE utf8mb4_unicode_ci");
    let rows = db.rows("SELECT * FROM t");
    assert_eq!(rows.len(), 1);
}

// ── ALTER TABLE AUTO_INCREMENT = N (accepted, no-op) ─────────────────────────

#[test]
fn test_alter_table_set_auto_increment_noop() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t (id INT AUTO_INCREMENT NOT NULL, val TEXT, PRIMARY KEY (id))");
    db.exec("INSERT INTO t (val) VALUES ('a')");
    // Should be accepted without error
    db.exec("ALTER TABLE t AUTO_INCREMENT = 100");
    // Table should still work
    let rows = db.rows("SELECT * FROM t");
    assert_eq!(rows.len(), 1);
}

// ── SHOW WARNINGS / SHOW ERRORS ───────────────────────────────────────────────

#[test]
fn test_show_warnings_noop() {
    let mut db = Db::new();
    // Should return empty, not error
    let result = db.run("SHOW WARNINGS").unwrap();
    assert!(matches!(
        result,
        QueryResult::Empty | QueryResult::Rows { .. }
    ));
}

#[test]
fn test_show_errors_noop() {
    let mut db = Db::new();
    let result = db.run("SHOW ERRORS").unwrap();
    assert!(matches!(
        result,
        QueryResult::Empty | QueryResult::Rows { .. }
    ));
}

// ── ALTER TABLE ADD KEY (synonym) ─────────────────────────────────────────────

#[test]
fn test_alter_table_add_key_synonym() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t (id INT, val INT)");
    // MySQL: KEY is a synonym for INDEX
    db.exec("ALTER TABLE t ADD KEY idx_val (val)");
    let rows = db.rows("SHOW INDEX FROM t");
    let names: Vec<_> = rows
        .iter()
        .map(|r| match &r[2] {
            Value::Text(s) => s.clone(),
            _ => String::new(),
        })
        .collect();
    assert!(
        names.contains(&"idx_val".to_string()),
        "Expected idx_val from ADD KEY: {names:?}"
    );
}

// ── RENAME TABLE using ALTER TABLE RENAME TO (existing path, regression) ──────

#[test]
fn test_alter_table_rename_to_regression() {
    let mut db = Db::new();
    db.exec("CREATE TABLE t_old (id INT)");
    db.exec("ALTER TABLE t_old RENAME TO t_new");
    let err = db.run("SELECT * FROM t_old").unwrap_err();
    assert!(matches!(err, DbError::TableNotFound { .. }));
    db.exec("SELECT * FROM t_new");
}
