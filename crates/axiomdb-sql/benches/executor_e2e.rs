//! End-to-end executor benchmarks using MmapStorage + WAL.
//!
//! Measures the full pipeline: parse → analyze → execute → WAL → MmapStorage.
//! These are the most honest numbers: exactly what a real AxiomDB workload costs.
//!
//! Run with:
//!   cargo bench --bench executor_e2e -p axiomdb-sql

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_sql::{
    analyze, bloom::BloomRegistry, execute, execute_with_ctx, parse, SessionContext,
};
use axiomdb_storage::{MemoryStorage, MmapStorage};
use axiomdb_wal::TxnManager;
use criterion::{criterion_group, criterion_main, BatchSize, Criterion, Throughput};

// ── Helpers ───────────────────────────────────────────────────────────────────

struct Db {
    storage: MmapStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
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

/// Multi-row INSERT: one SQL string with N VALUES rows.
///
/// Measures the full pipeline with one parse + one analyze + one execute
/// for all N rows. Uses record_insert_batch (Phase 3.17) internally, so
/// WAL writes are O(1) instead of O(N).
///
/// Compare with `bench_insert_batch_transaction` (N separate SQL strings in
/// one txn) to isolate the parse/analyze overhead per SQL string.
fn bench_insert_multi_row(c: &mut Criterion) {
    let sizes = [100u32, 1_000, 10_000];
    let mut group = c.benchmark_group("insert_multi_row");

    for &n in &sizes {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(
            criterion::BenchmarkId::new("axiomdb_mmap_wal", n),
            &n,
            |b, &n| {
                // Build the multi-row SQL once per benchmark (not included in timing).
                let sql = {
                    let mut s = String::from("INSERT INTO t VALUES ");
                    for i in 1..=n {
                        if i > 1 {
                            s.push(',');
                        }
                        s.push_str(&format!("({i},'row{i}')"));
                    }
                    s
                };

                b.iter_batched(
                    || {
                        let mut db = Db::open();
                        db.run("CREATE TABLE t (id INT NOT NULL, val TEXT)");
                        db
                    },
                    |mut db| {
                        // One parse + one analyze + one execute for all N rows.
                        db.run(&sql);
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

    // ── Clustered table aggregate (PRIMARY KEY → clustered B-tree) ────────────
    // Mirrors the wire benchmark: bench_users has id INT PK, name TEXT, age INT,
    // active BOOL, score DOUBLE, email TEXT — only age+score needed for aggregate.
    group.bench_function("group_by_age_avg_score_clustered_50k", |b| {
        let db = std::sync::Mutex::new({
            let mut d = Db::open();
            d.run("CREATE TABLE bench_c (id INT NOT NULL PRIMARY KEY, name TEXT NOT NULL, age INT NOT NULL, active BOOL NOT NULL, score DOUBLE NOT NULL, email TEXT NOT NULL)");
            for i in 1u32..=50_000 {
                d.run(&format!(
                    "INSERT INTO bench_c VALUES ({i}, 'User_{i:05}', {}, {}, {:.1}, 'u{i}@x.com')",
                    20 + (i % 62),
                    if i % 2 == 0 { "TRUE" } else { "FALSE" },
                    (i % 100) as f64,
                ));
            }
            d
        });
        b.iter(|| {
            db.lock().unwrap().run(
                "SELECT age, COUNT(*) AS c, AVG(score) AS a FROM bench_c GROUP BY age ORDER BY age",
            );
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

fn bench_clustered_update(c: &mut Criterion) {
    let mut group = c.benchmark_group("clustered_update");

    // Single-row point UPDATE by PK — zero-alloc fast path (all fixed-size SET cols).
    group.throughput(Throughput::Elements(1));
    group.bench_function("point_update_pk_fixed", |b| {
        b.iter_batched(
            || {
                let mut db = Db::open();
                db.run(
                    "CREATE TABLE players (id INT PRIMARY KEY, level INT, points INT, active BOOL)",
                );
                for i in 1..=1000_u32 {
                    db.run(&format!("INSERT INTO players VALUES ({i}, 1, 0, TRUE)"));
                }
                db
            },
            |mut db| {
                db.run("UPDATE players SET level = level + 1, points = points + 10 WHERE id = 500");
            },
            BatchSize::SmallInput,
        );
    });

    // Batch UPDATE — 100 rows by range scan (fixed-size SET cols, zero-alloc path).
    group.throughput(Throughput::Elements(100));
    group.bench_function("batch_update_100_fixed", |b| {
        b.iter_batched(
            || {
                let mut db = Db::open();
                db.run(
                    "CREATE TABLE players (id INT PRIMARY KEY, level INT, points INT, active BOOL)",
                );
                for i in 1..=1000_u32 {
                    db.run(&format!("INSERT INTO players VALUES ({i}, 1, 0, TRUE)"));
                }
                db
            },
            |mut db| {
                // id 1..=100
                db.run("UPDATE players SET level = level + 1 WHERE id >= 1 AND id <= 100");
            },
            BatchSize::SmallInput,
        );
    });

    // Steady-state UPDATE — reuse same db (no setup alloc amortised into setup).
    group.throughput(Throughput::Elements(1));
    group.bench_function("steady_state_point_update", |b| {
        let mut db = Db::open();
        db.run("CREATE TABLE players (id INT PRIMARY KEY, level INT, points INT, active BOOL)");
        for i in 1..=1000_u32 {
            db.run(&format!("INSERT INTO players VALUES ({i}, 1, 0, TRUE)"));
        }
        let mut counter = 0i32;
        b.iter(|| {
            counter += 1;
            let id = ((counter - 1) % 1000) + 1;
            db.run(&format!(
                "UPDATE players SET level = {counter} WHERE id = {id}"
            ));
        });
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

/// INSERT batch with MemoryStorage (no mmap, no disk I/O).
///
/// Establishes a baseline: any overhead vs `insert_batch_cached` is pure mmap cost.
fn bench_insert_batch_memory(c: &mut Criterion) {
    let sizes = [1_000u32, 10_000];
    let mut group = c.benchmark_group("insert_batch_memory");

    for &n in &sizes {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(
            criterion::BenchmarkId::new("axiomdb_memory_wal", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || {
                        let dir = tempfile::tempdir().unwrap();
                        let wal_path = dir.path().join("bench.wal");
                        let mut storage = MemoryStorage::new();
                        CatalogBootstrap::init(&mut storage).unwrap();
                        let txn = TxnManager::create(&wal_path).unwrap();
                        let mut ctx = SessionContext::new();
                        let mut bloom = BloomRegistry::new();
                        let mut db_mem = (storage, txn, dir);
                        let snap = db_mem
                            .1
                            .active_snapshot()
                            .unwrap_or_else(|_| db_mem.1.snapshot());
                        let stmt = parse("CREATE TABLE t (id INT, val TEXT)", None).unwrap();
                        let analyzed = analyze(stmt, &db_mem.0, snap).unwrap();
                        execute_with_ctx(
                            analyzed,
                            &mut db_mem.0,
                            &mut db_mem.1,
                            &mut bloom,
                            &mut ctx,
                        )
                        .unwrap();
                        (db_mem, ctx, bloom)
                    },
                    |((mut storage, mut txn, _dir), mut ctx, mut bloom)| {
                        execute_with_ctx(
                            analyze(parse("BEGIN", None).unwrap(), &storage, txn.snapshot())
                                .unwrap(),
                            &mut storage,
                            &mut txn,
                            &mut bloom,
                            &mut ctx,
                        )
                        .unwrap();
                        for i in 1..=n {
                            let sql = format!("INSERT INTO t VALUES ({i}, 'row{i}')");
                            let snap = txn.active_snapshot().unwrap();
                            let analyzed =
                                analyze(parse(&sql, None).unwrap(), &storage, snap).unwrap();
                            execute_with_ctx(
                                analyzed,
                                &mut storage,
                                &mut txn,
                                &mut bloom,
                                &mut ctx,
                            )
                            .unwrap();
                        }
                        execute_with_ctx(
                            analyze(
                                parse("COMMIT", None).unwrap(),
                                &storage,
                                txn.active_snapshot().unwrap(),
                            )
                            .unwrap(),
                            &mut storage,
                            &mut txn,
                            &mut bloom,
                            &mut ctx,
                        )
                        .unwrap();
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// INSERT batch with SessionContext schema cache enabled.
///
/// This is the most realistic benchmark for a long-lived connection:
/// the schema is cached after the first lookup, so repeated INSERTs
/// do not re-scan the catalog heap on every statement.
///
/// Compare with `insert_batch_transaction` (no cache) to isolate the
/// catalog scan overhead per statement.
fn bench_insert_batch_cached(c: &mut Criterion) {
    let sizes = [100u32, 1_000, 10_000];
    let mut group = c.benchmark_group("insert_batch_cached");

    for &n in &sizes {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_with_input(
            criterion::BenchmarkId::new("axiomdb_mmap_wal_ctx", n),
            &n,
            |b, &n| {
                b.iter_batched(
                    || {
                        let mut db = Db::open();
                        db.run("CREATE TABLE t (id INT, val TEXT)");
                        db
                    },
                    |mut db| {
                        let mut ctx = SessionContext::new();
                        db.run_ctx("BEGIN", &mut ctx);
                        for i in 1..=n {
                            db.run_ctx(&format!("INSERT INTO t VALUES ({i}, 'row{i}')"), &mut ctx);
                        }
                        db.run_ctx("COMMIT", &mut ctx);
                    },
                    BatchSize::SmallInput,
                );
            },
        );
    }

    group.finish();
}

/// Baseline for group commit throughput comparison.
///
/// Measures INSERT throughput with inline fsync (group commit disabled)
/// for 1, 4, 8, and 16 serial threads. Use this as the denominator when
/// comparing against group commit enabled results.
///
/// To test group commit, enable it in server config and measure the same
/// workload with N concurrent connections; expected improvement: ~N×
/// throughput for N concurrent writers sharing one fsync per interval.
fn bench_insert_serial_fsync_baseline(c: &mut Criterion) {
    let counts = [1u32, 4, 8, 16];
    let mut group = c.benchmark_group("insert_serial_fsync_baseline");

    for n in counts {
        group.throughput(Throughput::Elements(n as u64));
        group.bench_function(format!("{n}_inserts"), |b| {
            b.iter_batched(
                || {
                    let mut db = Db::open();
                    db.run("CREATE TABLE gc_bench (id INT NOT NULL, v TEXT)");
                    db
                },
                |mut db| {
                    for i in 0..n {
                        db.run(&format!("INSERT INTO gc_bench VALUES ({i}, 'val{i}')"));
                    }
                },
                BatchSize::SmallInput,
            );
        });
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_insert_single,
    bench_insert_sequential,
    bench_insert_batch_txn,
    bench_insert_multi_row,
    bench_insert_batch_memory,
    bench_insert_batch_cached,
    bench_select_full_scan,
    bench_select_where_filter,
    bench_select_count_aggregate,
    bench_update_where,
    bench_clustered_update,
    bench_full_pipeline,
    bench_insert_serial_fsync_baseline,
);
criterion_main!(benches);
