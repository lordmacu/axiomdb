//! End-to-end executor benchmarks using MmapStorage + WAL.
//!
//! Measures the full pipeline: parse → analyze → execute → WAL → MmapStorage.
//! These are the most honest numbers: exactly what a real AxiomDB workload costs.
//!
//! Run with:
//!   cargo bench --bench executor_e2e -p axiomdb-sql

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_sql::{analyze, execute, parse};
use axiomdb_storage::MmapStorage;
use axiomdb_wal::TxnManager;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

// ── Helpers ───────────────────────────────────────────────────────────────────

struct Db {
    storage: MmapStorage,
    txn: TxnManager,
    _dir: tempfile::TempDir, // keeps tempdir alive
}

impl Db {
    fn open() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("bench.db");
        let wal_path = dir.path().join("bench.wal");
        let mut storage = MmapStorage::create(&db_path).unwrap();
        CatalogBootstrap::init(&mut storage).unwrap();
        let txn = TxnManager::create(&wal_path).unwrap();
        Db {
            storage,
            txn,
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
}

/// Creates a fresh database with a `users` table pre-populated with `n` rows.
fn db_with_users(n: u32) -> Db {
    let mut db = Db::open();
    db.run("CREATE TABLE users (id INT NOT NULL, name TEXT, age INT, active BOOL)");
    for i in 1..=n {
        db.run(&format!(
            "INSERT INTO users VALUES ({i}, 'user{i}', {}, TRUE)",
            20 + (i % 50)
        ));
    }
    db
}

// ── Benchmarks ────────────────────────────────────────────────────────────────

fn bench_insert_single(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_single_row");

    // Single autocommit INSERT per iteration (parse + analyze + execute + WAL + mmap).
    group.throughput(Throughput::Elements(1));
    group.bench_function("axiomdb_mmap_wal", |b| {
        b.iter_batched(
            || {
                let mut db = Db::open();
                db.run("CREATE TABLE t (id INT, val TEXT)");
                db
            },
            |mut db| {
                db.run("INSERT INTO t VALUES (1, 'hello world')");
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_insert_sequential(c: &mut Criterion) {
    let sizes = [100u32, 1_000, 10_000];
    let mut group = c.benchmark_group("insert_sequential");

    for &n in &sizes {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(
            criterion::BenchmarkId::new("axiomdb_mmap_wal", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || {
                        let mut db = Db::open();
                        db.run("CREATE TABLE t (id INT, val TEXT)");
                        db
                    },
                    |mut db| {
                        // Insert n rows, each as a separate autocommit statement.
                        for i in 1..=n {
                            db.run(&format!("INSERT INTO t VALUES ({i}, 'row{i}')"));
                        }
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_insert_batch_txn(c: &mut Criterion) {
    // All inserts inside a single explicit transaction — reduces WAL fsync overhead.
    let sizes = [100u32, 1_000, 10_000];
    let mut group = c.benchmark_group("insert_batch_transaction");

    for &n in &sizes {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(
            criterion::BenchmarkId::new("axiomdb_mmap_wal", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || {
                        let mut db = Db::open();
                        db.run("CREATE TABLE t (id INT, val TEXT)");
                        db
                    },
                    |mut db| {
                        db.run("BEGIN");
                        for i in 1..=n {
                            db.run(&format!("INSERT INTO t VALUES ({i}, 'row{i}')"));
                        }
                        db.run("COMMIT");
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

fn bench_select_full_scan(c: &mut Criterion) {
    let sizes = [100u32, 1_000, 10_000];
    let mut group = c.benchmark_group("select_full_scan");

    for &n in &sizes {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(
            criterion::BenchmarkId::new("axiomdb_mmap_wal", n),
            &n,
            |b, &n| {
                // Pre-populate once, measure repeated scans.
                let db = std::sync::Mutex::new(db_with_users(n));
                b.iter(|| {
                    db.lock().unwrap().run("SELECT * FROM users");
                });
            },
        );
    }

    group.finish();
}

fn bench_select_where_filter(c: &mut Criterion) {
    let mut group = c.benchmark_group("select_where_filter");

    // 10K rows, filter to ~200 (age = 30, ~2% selectivity).
    group.throughput(Throughput::Elements(10_000));
    group.bench_function("axiomdb_mmap_wal_10k", |b| {
        let db = std::sync::Mutex::new(db_with_users(10_000));
        b.iter(|| {
            db.lock().unwrap().run("SELECT * FROM users WHERE age = 30");
        });
    });

    group.finish();
}

fn bench_select_count_aggregate(c: &mut Criterion) {
    let mut group = c.benchmark_group("select_aggregate");

    group.bench_function("count_star_10k", |b| {
        let db = std::sync::Mutex::new(db_with_users(10_000));
        b.iter(|| {
            db.lock().unwrap().run("SELECT COUNT(*) FROM users");
        });
    });

    group.bench_function("group_by_age_10k", |b| {
        let db = std::sync::Mutex::new(db_with_users(10_000));
        b.iter(|| {
            db.lock()
                .unwrap()
                .run("SELECT age, COUNT(*) FROM users GROUP BY age");
        });
    });

    group.finish();
}

fn bench_update_where(c: &mut Criterion) {
    let mut group = c.benchmark_group("update_where");

    // UPDATE ~200 rows out of 10K.
    group.throughput(Throughput::Elements(200));
    group.bench_function("axiomdb_update_200_of_10k", |b| {
        b.iter_batched(
            || db_with_users(10_000),
            |mut db| {
                db.run("UPDATE users SET active = FALSE WHERE age = 30");
            },
            BatchSize::SmallInput,
        );
    });

    group.finish();
}

fn bench_full_pipeline(c: &mut Criterion) {
    // Measures parse + analyze + execute as a single unit for INSERT.
    // Shows per-statement overhead of the executor above the storage layer.
    let mut group = c.benchmark_group("full_pipeline_overhead");

    group.throughput(Throughput::Elements(1));
    group.bench_function("insert_1_row_parse_to_disk", |b| {
        let db = std::sync::Mutex::new({
            let mut d = Db::open();
            d.run("CREATE TABLE t (id INT, name TEXT, val INT)");
            d
        });
        let sqls: Vec<String> = (1..=1000)
            .map(|i| format!("INSERT INTO t VALUES ({i}, 'name{i}', {i})"))
            .collect();
        let mut idx = 0usize;
        b.iter(|| {
            let sql = &sqls[idx % sqls.len()];
            idx += 1;
            db.lock().unwrap().run(sql);
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_insert_single,
    bench_insert_sequential,
    bench_insert_batch_txn,
    bench_select_full_scan,
    bench_select_where_filter,
    bench_select_count_aggregate,
    bench_update_where,
    bench_full_pipeline,
);
criterion_main!(benches);
