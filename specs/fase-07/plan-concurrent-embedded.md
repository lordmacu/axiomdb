# Plan: concurrent-embedded — SharedDb + Connection types

Phase: 7 (deferred) — Concurrent embedded connections
Task: SharedDb + Connection (Task 1 of sprint concurrent-embedded)
Spec: specs/fase-07/spec-concurrent-embedded.md
Status: done

## Summary

Creates `crates/axiomdb-embedded/src/shared_db.rs` with `SharedDbInner`,
`SharedDb` (cloneable, `Arc`-wrapped engine handle), and `Connection`
(per-connection `SessionContext` + `SchemaCache`). The execution path mirrors
`SharedDatabase::execute_query` from the wire server — no global lock for reads,
`catalog_lock` write-guard for DDL only. `Db` and `AsyncDb` are untouched.

Four steps: (1) shared engine structs + open API, (2) Connection execution core +
Drop, (3) explicit transaction methods, (4) re-exports + tests + final validation.

## Dependencies

Must be done first:
- [x] spec-concurrent-embedded approved

Blocks (until this plan is done):
- [ ] Task 2 — AsyncSharedDb wrapper
- [ ] Task 3 — concurrency tests (integration)
- [ ] Task 4 — close subphase

## Affected files

New files:
- `crates/axiomdb-embedded/src/shared_db.rs` — SharedDbInner, SharedDb, Connection

Modified files:
- `crates/axiomdb-embedded/src/lib.rs` — add `pub mod shared_db; pub use shared_db::{SharedDb, Connection};`

---

## Step 1 — SharedDbInner + SharedDb (open API + connect)

**Goal:** Compilable `SharedDb` with `open()`, `open_memory()`, `open_dsn()`, `connect()`.
**Files:** `crates/axiomdb-embedded/src/shared_db.rs` (new), `src/lib.rs` (mod declaration)

### Implementation outline

```rust
// crates/axiomdb-embedded/src/shared_db.rs

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use axiomdb_catalog::bootstrap::CatalogBootstrap;
use axiomdb_core::error::DbError;
use axiomdb_sql::{bloom::BloomRegistry, SchemaCache, SessionContext};
use axiomdb_storage::MmapStorage;
use axiomdb_wal::TxnManager;

use crate::db::resolve_local_dsn_path;   // already exists in lib.rs

pub type Row = crate::db::Row;  // re-use existing alias

struct SharedDbInner {
    storage: MmapStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
    schema_version: AtomicU64,
    catalog_lock: RwLock<()>,
    _tmpdir: Option<tempfile::TempDir>,
}

#[derive(Clone)]
pub struct SharedDb {
    inner: Arc<SharedDbInner>,
}

impl SharedDb {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, DbError> { ... }
    pub fn open_memory() -> Result<Self, DbError> { ... }
    pub fn open_dsn(dsn: impl Into<String>) -> Result<Self, DbError> { ... }
    pub fn connect(&self) -> Connection {
        Connection {
            inner: Arc::clone(&self.inner),
            session: SessionContext::default(),
            schema_cache: SchemaCache::default(),
            schema_version_seen: 0,
        }
    }
}

pub struct Connection {
    inner: Arc<SharedDbInner>,
    session: SessionContext,
    schema_cache: SchemaCache,
    schema_version_seen: u64,
}
```

### Verification

```bash
./tools/vm.sh build -p axiomdb-embedded
```

### Commit

```
feat(fase-07): add SharedDb + SharedDbInner skeleton

Step 1 of specs/fase-07/plan-concurrent-embedded.md
```

---

## Step 2 — Connection execution core + Drop

**Goal:** `Connection::execute()`, `query()`, `query_with_columns()`, `run_inner()`, `Drop`.
**Files:** `shared_db.rs` (extend Connection impl)

### Test to add

```rust
// in shared_db.rs #[cfg(test)] module
#[test]
fn connection_basic_smoke() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path().join("t.db")).unwrap();
    let mut conn = db.connect();
    conn.execute("CREATE TABLE t (id INT, v TEXT)").unwrap();
    conn.execute("INSERT INTO t VALUES (1, 'hello')").unwrap();
    let rows = conn.query("SELECT id, v FROM t").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[0][1], Value::Text("hello".into()));
}

#[test]
fn two_connections_share_data() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path().join("t.db")).unwrap();
    let mut conn1 = db.connect();
    let mut conn2 = db.connect();
    conn1.execute("CREATE TABLE t (id INT)").unwrap();
    conn1.execute("INSERT INTO t VALUES (42)").unwrap();
    let rows = conn2.query("SELECT id FROM t").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(42));
}

#[test]
fn connection_drop_rolls_back_open_txn() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path().join("t.db")).unwrap();
    {
        let mut conn = db.connect();
        conn.execute("CREATE TABLE t (id INT)").unwrap();
        conn.begin().unwrap();
        conn.execute("INSERT INTO t VALUES (1)").unwrap();
        // drop without commit → rollback
    }
    let mut conn2 = db.connect();
    let rows = conn2.query("SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 0);
}
```

### Implementation outline

```rust
impl Connection {
    fn maybe_invalidate_schema_cache(&mut self) {
        let current = self.inner.schema_version.load(Ordering::Acquire);
        if current != self.schema_version_seen {
            self.schema_cache.invalidate();
            self.schema_version_seen = current;
        }
    }

    fn run_inner(&mut self, sql: &str) -> Result<QueryResult, DbError> {
        self.maybe_invalidate_schema_cache();

        if axiomdb_sql::sql_starts_with_select_keyword(sql) {
            // Pure read path — no catalog lock, read_only=true for concurrent safety.
            return axiomdb_sql::statement_cache::run_cached(
                sql,
                &self.inner.storage,
                &self.inner.txn,
                &self.inner.bloom,
                &mut self.schema_cache,
                &mut self.session,
                // read_only=true: concurrent &self read path, no staged writes
                true,
            );
        }

        // Parse to check if DDL
        let stmt = axiomdb_sql::parse_with_sql_mode(sql, None, self.session.sql_mode_flags())?;
        let is_ddl = is_schema_changing(&stmt);

        let snap = if let Some(ref ct) = self.session.conn_txn {
            self.inner.txn.active_snapshot(ct)
        } else {
            self.inner.txn.snapshot()
        };
        let analyzed = axiomdb_sql::analyze_cached(
            stmt, &self.inner.storage, snap, &mut self.schema_cache
        )?;

        // DDL: take catalog write lock for schema mutation.
        // DML: no outer lock — per-page locks in MmapStorage handle concurrency.
        let _guard = if is_ddl {
            Some(self.inner.catalog_lock.write()
                .map_err(|_| DbError::Other("catalog lock poisoned".into()))?)
        } else {
            None
        };

        let result = axiomdb_sql::execute_with_ctx(
            analyzed,
            &self.inner.storage,
            &self.inner.txn,
            &self.inner.bloom,
            &mut self.session,
        )?;

        if is_ddl {
            self.inner.schema_version.fetch_add(1, Ordering::Release);
            self.schema_version_seen = self.inner.schema_version.load(Ordering::Acquire);
            self.schema_cache.invalidate();
        }

        Ok(result)
    }

    pub fn execute(&mut self, sql: &str) -> Result<u64, DbError> {
        let result = self.run_inner(sql)?;
        Ok(match result {
            QueryResult::Affected { count, .. } => count,
            QueryResult::Rows { rows, .. } => rows.len() as u64,
            QueryResult::Empty => 0,
        })
    }

    pub fn query(&mut self, sql: &str) -> Result<Vec<Row>, DbError> {
        let result = self.run_inner(sql)?;
        Ok(match result { QueryResult::Rows { rows, .. } => rows, _ => vec![] })
    }

    pub fn query_with_columns(&mut self, sql: &str) -> Result<(Vec<String>, Vec<Row>), DbError> {
        let result = self.run_inner(sql)?;
        Ok(match result {
            QueryResult::Rows { columns, rows } => {
                (columns.into_iter().map(|c| c.name).collect(), rows)
            }
            _ => (vec![], vec![]),
        })
    }

    pub fn in_transaction(&self) -> bool { self.session.conn_txn.is_some() }
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Some(conn) = self.session.conn_txn.take() {
            let _ = self.inner.txn.rollback(conn, &self.inner.storage);
        }
    }
}

fn is_schema_changing(stmt: &axiomdb_sql::ast::Stmt) -> bool {
    use axiomdb_sql::ast::Stmt;
    matches!(
        stmt,
        Stmt::CreateTable(_) | Stmt::CreateDatabase(_) | Stmt::CreateIndex(_)
        | Stmt::DropTable(_) | Stmt::DropDatabase(_) | Stmt::DropIndex(_)
        | Stmt::AlterTable(_) | Stmt::TruncateTable(_)
    )
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded
```

### Commit

```
feat(fase-07): Connection execution core — execute/query/run_inner/Drop

Step 2 of specs/fase-07/plan-concurrent-embedded.md
```

---

## Step 3 — Explicit transaction methods

**Goal:** `begin()`, `commit()`, `rollback()` with correct error cases.
**Files:** `shared_db.rs` (extend Connection impl)

### Tests to add

```rust
#[test]
fn explicit_txn_commit() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path().join("t.db")).unwrap();
    let mut conn = db.connect();
    conn.execute("CREATE TABLE t (id INT)").unwrap();
    conn.begin().unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();
    conn.commit().unwrap();
    let rows = conn.query("SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn explicit_txn_rollback() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path().join("t.db")).unwrap();
    let mut conn = db.connect();
    conn.execute("CREATE TABLE t (id INT)").unwrap();
    conn.begin().unwrap();
    conn.execute("INSERT INTO t VALUES (1)").unwrap();
    conn.rollback().unwrap();
    let rows = conn.query("SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 0);
}

#[test]
fn begin_while_in_txn_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path().join("t.db")).unwrap();
    let mut conn = db.connect();
    conn.begin().unwrap();
    assert!(conn.begin().is_err());
}

#[test]
fn commit_without_txn_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path().join("t.db")).unwrap();
    let mut conn = db.connect();
    assert!(conn.commit().is_err());
}

#[test]
fn rollback_without_txn_is_error() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path().join("t.db")).unwrap();
    let mut conn = db.connect();
    assert!(conn.rollback().is_err());
}
```

### Implementation outline

```rust
impl Connection {
    pub fn begin(&mut self) -> Result<(), DbError> {
        if self.session.conn_txn.is_some() {
            return Err(DbError::Other("already in transaction".into()));
        }
        self.session.conn_txn = Some(self.inner.txn.begin()?);
        self.session.in_explicit_txn = true;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<(), DbError> {
        let conn = self.session.conn_txn.take()
            .ok_or_else(|| DbError::Other("no active transaction".into()))?;
        self.session.in_explicit_txn = false;
        self.inner.txn.commit(conn)?;
        Ok(())
    }

    pub fn rollback(&mut self) -> Result<(), DbError> {
        let conn = self.session.conn_txn.take()
            .ok_or_else(|| DbError::Other("no active transaction".into()))?;
        self.session.in_explicit_txn = false;
        self.inner.txn.rollback(conn, &self.inner.storage)?;
        Ok(())
    }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-embedded
```

### Commit

```
feat(fase-07): Connection::begin/commit/rollback with error cases

Step 3 of specs/fase-07/plan-concurrent-embedded.md
```

---

## Step 4 — Re-exports, DDL cross-connection test, final validation

**Goal:** Public API accessible from crate root; schema version invalidation proven; all spec done criteria checked.
**Files:** `src/lib.rs` (add re-exports), `shared_db.rs` (concurrent + schema tests)

### Tests to add

```rust
#[test]
fn concurrent_reads_no_deadlock() {
    use std::sync::Arc;
    use std::thread;

    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path().join("t.db")).unwrap();
    let mut setup = db.connect();
    setup.execute("CREATE TABLE t (id INT)").unwrap();
    for i in 0..100 {
        setup.execute(&format!("INSERT INTO t VALUES ({i})")).unwrap();
    }
    drop(setup);

    let db = Arc::new(db);
    let handles: Vec<_> = (0..8).map(|_| {
        let db = Arc::clone(&db);
        thread::spawn(move || {
            let mut conn = db.connect();
            for _ in 0..20 {
                let rows = conn.query("SELECT COUNT(*) FROM t").unwrap();
                assert_eq!(rows[0][0], axiomdb_types::Value::BigInt(100));
            }
        })
    }).collect();

    for h in handles { h.join().unwrap(); }
}

#[test]
fn ddl_on_conn_a_visible_to_conn_b() {
    let dir = tempfile::tempdir().unwrap();
    let db = SharedDb::open(dir.path().join("t.db")).unwrap();
    let mut conn_a = db.connect();
    let mut conn_b = db.connect();

    conn_a.execute("CREATE TABLE t (id INT)").unwrap();
    conn_a.execute("INSERT INTO t VALUES (1)").unwrap();

    // conn_b's schema cache is stale — schema_version should trigger invalidation
    let rows = conn_b.query("SELECT id FROM t").unwrap();
    assert_eq!(rows.len(), 1);
}

#[test]
fn shared_db_clone_is_same_database() {
    let dir = tempfile::tempdir().unwrap();
    let db1 = SharedDb::open(dir.path().join("t.db")).unwrap();
    let db2 = db1.clone();
    let mut c1 = db1.connect();
    let mut c2 = db2.connect();
    c1.execute("CREATE TABLE t (id INT)").unwrap();
    c1.execute("INSERT INTO t VALUES (99)").unwrap();
    let rows = c2.query("SELECT id FROM t").unwrap();
    assert_eq!(rows[0][0], axiomdb_types::Value::Int(99));
}
```

### lib.rs changes

```rust
// in crates/axiomdb-embedded/src/lib.rs — add alongside existing pub mod db:
pub mod shared_db;
pub use shared_db::{Connection, SharedDb};
```

### Verification against spec done criteria

```bash
./tools/vm.sh test -p axiomdb-embedded     # all tests pass including Db
./tools/vm.sh clippy                        # -D warnings clean
./tools/vm.sh build -p axiomdb-embedded    # compiles
```

- [ ] `SharedDb: Clone + Send + Sync` — verify with `static_assertions` or compile test
- [ ] `Connection: Send` — verified by moving into thread in concurrent test
- [ ] All public items have rustdoc
- [ ] `Drop` rolls back open txn — `connection_drop_rolls_back_open_txn` test
- [ ] Schema cache invalidation cross-connection — `ddl_on_conn_a_visible_to_conn_b` test
- [ ] Existing `Db` tests unaffected

### Commit

```
feat(fase-07): SharedDb+Connection public API, concurrent read tests

Implements specs/fase-07/spec-concurrent-embedded.md
Plan: specs/fase-07/plan-concurrent-embedded.md
Tests: 10 new tests (smoke, concurrent, schema-version, txn error cases)
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|---|---|---|
| `analyze_cached` vs `analyze_cached_with_defaults` signature mismatch | low | check import at Step 2, use whichever the embedded `Db::run_inner` already uses |
| `catalog_lock` poisoned on DDL panic | low | map `PoisonError` to `DbError::Other` |
| `run_cached` with `read_only=true` inside explicit write txn returns stale data | medium | document: callers must use `execute()` for DML; `query()` uses read-only path which is safe because it reads committed snapshot, not staged writes |

## Rollback plan

If abandoned mid-way: delete `shared_db.rs` and remove the `pub mod shared_db` line from `lib.rs`. `Db` is untouched — zero regression.

## Estimated effort

Total: ~4 hours
- Step 1: 30 min (struct scaffolding + open wiring)
- Step 2: 90 min (execution core — most logic here)
- Step 3: 30 min (transaction methods — mechanical)
- Step 4: 60 min (concurrent tests + final validation)
