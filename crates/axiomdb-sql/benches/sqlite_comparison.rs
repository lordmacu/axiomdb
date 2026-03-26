//! SQLite vs AxiomDB benchmark: INSERT, SELECT, UPDATE, DELETE.
//!
//! Both engines run with WAL-backed durability on real disk (tempdir).
//! SQLite: journal_mode=WAL + synchronous=NORMAL.
//! AxiomDB: MmapStorage + TxnManager (WAL).
//!
//! Run with:
//!   cargo bench --bench sqlite_comparison -p axiomdb-sql

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_sql::{
    analyze, bloom::BloomRegistry, execute, execute_with_ctx, parse, SessionContext,
};
use axiomdb_storage::MmapStorage;
use axiomdb_wal::TxnManager;
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use rusqlite::Connection;

// ── AxiomDB helpers ───────────────────────────────────────────────────────────

struct AxiomDb {
    storage: MmapStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
    _dir: tempfile::TempDir,
}

impl AxiomDb {
    fn open() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("bench.db");
        let wal_path = dir.path().join("bench.wal");
        let mut storage = MmapStorage::create(&db_path).unwrap();
        CatalogBootstrap::init(&mut storage).unwrap();
        let txn = TxnManager::create(&wal_path).unwrap();
        AxiomDb {
            storage,
            txn,
            bloom: BloomRegistry::new(),
            _dir: dir,
        }
    }

    fn run(&mut self, sql: &str) {
        let stmt = parse(sql, None).unwrap();
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = analyze(stmt, &self.storage, snap).unwrap();
        execute(analyzed, &mut self.storage, &mut self.txn).unwrap();
    }

    fn run_ctx(&mut self, sql: &str, ctx: &mut SessionContext) {
        let stmt = parse(sql, None).unwrap();
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = analyze(stmt, &self.storage, snap).unwrap();
        execute_with_ctx(
            analyzed,
            &mut self.storage,
            &mut self.txn,
            &mut self.bloom,
            ctx,
        )
        .unwrap();
    }

    fn setup_table(&mut self) {
        self.run("CREATE TABLE users (id INT NOT NULL, name TEXT, age INT, active BOOL)");
    }

    fn populate(&mut self, n: u32) {
        let mut ctx = SessionContext::new();
        self.run("BEGIN");
        for i in 1..=n {
            self.run_ctx(
                &format!(
                    "INSERT INTO users VALUES ({i}, 'user{i}', {}, TRUE)",
                    20 + (i % 50)
                ),
                &mut ctx,
            );
        }
        self.run("COMMIT");
    }
}

// ── SQLite helpers ────────────────────────────────────────────────────────────

fn open_sqlite() -> (Connection, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bench.db");
    let conn = Connection::open(&path).unwrap();
    conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
        .unwrap();
    (conn, dir)
}

fn sqlite_with_users(n: u32) -> (Connection, tempfile::TempDir) {
    let (conn, dir) = open_sqlite();
    conn.execute_batch(
        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER, active INTEGER);",
    )
    .unwrap();
    conn.execute_batch("BEGIN").unwrap();
    for i in 1..=n {
        conn.execute(
            "INSERT INTO users VALUES (?1, ?2, ?3, 1)",
            rusqlite::params![i as i64, format!("user{i}"), (20 + (i % 50)) as i64],
        )
        .unwrap();
    }
    conn.execute_batch("COMMIT").unwrap();
    (conn, dir)
}

// ── INSERT ────────────────────────────────────────────────────────────────────

fn bench_insert(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert");

    for &n in &[100u32, 1_000] {
        group.throughput(Throughput::Elements(n as u64));

        // AxiomDB: N inserts inside one explicit transaction
        group.bench_with_input(BenchmarkId::new("axiomdb", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut db = AxiomDb::open();
                    db.setup_table();
                    db
                },
                |mut db| {
                    let mut ctx = SessionContext::new();
                    db.run("BEGIN");
                    for i in 1..=n {
                        db.run_ctx(
                            &format!(
                                "INSERT INTO users VALUES ({i}, 'user{i}', {}, TRUE)",
                                20 + (i % 50)
                            ),
                            &mut ctx,
                        );
                    }
                    db.run("COMMIT");
                },
                BatchSize::SmallInput,
            );
        });

        // SQLite: N inserts inside one explicit transaction
        group.bench_with_input(BenchmarkId::new("sqlite", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let (conn, dir) = open_sqlite();
                    conn.execute_batch(
                        "CREATE TABLE users (id INTEGER PRIMARY KEY, name TEXT, age INTEGER, active INTEGER);",
                    )
                    .unwrap();
                    (conn, dir)
                },
                |(conn, _dir)| {
                    conn.execute_batch("BEGIN").unwrap();
                    for i in 1..=n {
                        conn.execute(
                            "INSERT INTO users VALUES (?1, ?2, ?3, 1)",
                            rusqlite::params![
                                i as i64,
                                format!("user{i}"),
                                (20 + (i % 50)) as i64
                            ],
                        )
                        .unwrap();
                    }
                    conn.execute_batch("COMMIT").unwrap();
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

// ── SELECT (full scan) ────────────────────────────────────────────────────────

fn bench_select(c: &mut Criterion) {
    let mut group = c.benchmark_group("select_full_scan");

    for &n in &[100u32, 1_000] {
        group.throughput(Throughput::Elements(n as u64));

        group.bench_with_input(BenchmarkId::new("axiomdb", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut db = AxiomDb::open();
                    db.setup_table();
                    db.populate(n);
                    db
                },
                |mut db| {
                    db.run("SELECT id, name, age FROM users");
                },
                BatchSize::SmallInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("sqlite", n), &n, |b, &n| {
            b.iter_batched(
                || sqlite_with_users(n),
                |(conn, _dir)| {
                    let mut stmt = conn.prepare("SELECT id, name, age FROM users").unwrap();
                    let _rows: Vec<(i64, String, i64)> = stmt
                        .query_map([], |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, i64>(2)?,
                            ))
                        })
                        .unwrap()
                        .map(|r| r.unwrap())
                        .collect();
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

// ── UPDATE ────────────────────────────────────────────────────────────────────

fn bench_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("update_where");

    for &n in &[100u32, 1_000] {
        // age=30 hits ~2% of rows (i % 50 == 10 → age=30)
        let hits = (n / 50).max(1);
        group.throughput(Throughput::Elements(hits as u64));

        group.bench_with_input(BenchmarkId::new("axiomdb", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut db = AxiomDb::open();
                    db.setup_table();
                    db.populate(n);
                    db
                },
                |mut db| {
                    db.run("UPDATE users SET active = FALSE WHERE age = 30");
                },
                BatchSize::SmallInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("sqlite", n), &n, |b, &n| {
            b.iter_batched(
                || sqlite_with_users(n),
                |(conn, _dir)| {
                    conn.execute("UPDATE users SET active = 0 WHERE age = 30", [])
                        .unwrap();
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

// ── DELETE ────────────────────────────────────────────────────────────────────

fn bench_delete(c: &mut Criterion) {
    let mut group = c.benchmark_group("delete_where");

    for &n in &[100u32, 1_000] {
        let hits = (n / 50).max(1);
        group.throughput(Throughput::Elements(hits as u64));

        group.bench_with_input(BenchmarkId::new("axiomdb", n), &n, |b, &n| {
            b.iter_batched(
                || {
                    let mut db = AxiomDb::open();
                    db.setup_table();
                    db.populate(n);
                    db
                },
                |mut db| {
                    db.run("DELETE FROM users WHERE age = 30");
                },
                BatchSize::SmallInput,
            );
        });

        group.bench_with_input(BenchmarkId::new("sqlite", n), &n, |b, &n| {
            b.iter_batched(
                || sqlite_with_users(n),
                |(conn, _dir)| {
                    conn.execute("DELETE FROM users WHERE age = 30", [])
                        .unwrap();
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_insert,
    bench_select,
    bench_update,
    bench_delete
);
criterion_main!(benches);
