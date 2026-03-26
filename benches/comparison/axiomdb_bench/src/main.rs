//! AxiomDB internal benchmark — runs INSIDE the AxiomDB container.
//! Uses real MmapStorage + WAL (no network overhead).
//! Outputs one JSON line per scenario to stdout.
//!
//! Usage (from host via docker exec):
//!   docker exec axiomdb_bench_axiomdb /bench/axiomdb_bench \
//!     --scenario insert_batch --rows 10000
//!
//! Comparison mode (AxiomDB vs SQLite, same process, no network):
//!   cargo run -p axiomdb-bench-comparison --release -- --compare --rows 5000
//!   cargo run -p axiomdb-bench-comparison --release -- --compare --rows 5000 --sqlite-memory

use std::path::Path;
use std::time::Instant;

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_sql::{
    analyze, analyze_cached, bloom::BloomRegistry, execute, execute_with_ctx, parse, QueryResult,
    SchemaCache, SessionContext,
};
use axiomdb_storage::MmapStorage;
use axiomdb_wal::TxnManager;
use rusqlite::Connection;

const WARMUP: usize = 2;
const RUNS: usize = 5;

// ── Engine ────────────────────────────────────────────────────────────────────

struct Db {
    storage: MmapStorage,
    txn: TxnManager,
    bloom: BloomRegistry,
}

impl Db {
    fn open(dir: &Path) -> Self {
        let db_path = dir.join("bench.db");
        let wal_path = dir.join("bench.wal");

        // Open existing or create fresh
        let mut storage = if db_path.exists() {
            MmapStorage::open(&db_path).expect("open storage")
        } else {
            let mut s = MmapStorage::create(&db_path).expect("create storage");
            CatalogBootstrap::init(&mut s).expect("bootstrap");
            s
        };

        // Bootstrap catalog if not yet initialized (fresh DB)
        let txn = if wal_path.exists() {
            TxnManager::open(&wal_path).expect("open WAL")
        } else {
            let t = TxnManager::create(&wal_path).expect("create WAL");
            // Ensure catalog is bootstrapped
            let _ = CatalogBootstrap::init(&mut storage);
            t
        };

        Self {
            storage,
            txn,
            bloom: BloomRegistry::new(),
        }
    }

    fn sql(&mut self, q: &str) -> QueryResult {
        let stmt = parse(q, None).unwrap_or_else(|e| panic!("parse({q}): {e}"));
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed =
            analyze(stmt, &self.storage, snap).unwrap_or_else(|e| panic!("analyze({q}): {e}"));
        execute(analyzed, &mut self.storage, &mut self.txn)
            .unwrap_or_else(|e| panic!("execute({q}): {e}"))
    }

    /// Like `sql()` but uses both schema caches:
    ///
    /// - `SchemaCache` in analyze() → skips catalog scan for repeated tables
    /// - `SessionContext` in execute_with_ctx() → skips executor-side catalog scan
    ///
    /// Pass the same `schema_cache` and `ctx` for the whole batch.
    fn sql_cached(
        &mut self,
        q: &str,
        schema_cache: &mut SchemaCache,
        ctx: &mut SessionContext,
    ) -> QueryResult {
        let stmt = parse(q, None).unwrap_or_else(|e| panic!("parse({q}): {e}"));
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = analyze_cached(stmt, &self.storage, snap, schema_cache)
            .unwrap_or_else(|e| panic!("analyze_cached({q}): {e}"));
        execute_with_ctx(
            analyzed,
            &mut self.storage,
            &mut self.txn,
            &mut self.bloom,
            ctx,
        )
        .unwrap_or_else(|e| panic!("execute_with_ctx({q}): {e}"))
    }

    fn sql_count(&mut self, q: &str) -> usize {
        match self.sql(q) {
            QueryResult::Rows { rows, .. } => rows.len(),
            _ => 0,
        }
    }
}

// ── Data ──────────────────────────────────────────────────────────────────────

fn gen_inserts(n: usize) -> Vec<String> {
    (1..=n).map(|i| {
        let active = if i % 2 == 0 { "TRUE" } else { "FALSE" };
        format!(
            "INSERT INTO bench_users VALUES ({i}, 'user_{i:06}', {age}, {active}, {score:.1}, 'u{i}@b.local')",
            age   = 18 + (i % 62),
            score = 100.0 + (i % 1000) as f64 * 0.1,
        )
    }).collect()
}

fn reset(db: &mut Db) {
    db.sql("DROP TABLE IF EXISTS bench_users");
    db.sql(
        "CREATE TABLE bench_users (
        id     INT  NOT NULL,
        name   TEXT NOT NULL,
        age    INT  NOT NULL,
        active BOOL NOT NULL,
        score  REAL NOT NULL,
        email  TEXT NOT NULL,
        PRIMARY KEY (id)
    )",
    );
}

/// Full load: reset then INSERT. Used for pre-loading data before read benchmarks
/// and for the diagnose helper. NOT used in timed INSERT scenarios.
fn load_batch(db: &mut Db, inserts: &[String]) {
    reset(db);
    insert_batch_pure(db, inserts);
}

/// INSERT only — reset happens outside timing via measure_with_setup.
/// Dual schema cache eliminates catalog heap scans (4×N → 2 total per batch).
fn insert_batch_pure(db: &mut Db, inserts: &[String]) {
    let mut schema_cache = SchemaCache::new();
    let mut ctx = SessionContext::default();
    db.sql("BEGIN");
    for sql in inserts {
        db.sql_cached(sql, &mut schema_cache, &mut ctx);
    }
    db.sql("COMMIT");
}

// ── Measurement ───────────────────────────────────────────────────────────────

fn measure<F: FnMut()>(mut f: F) -> f64 {
    for _ in 0..WARMUP {
        f();
    }
    let mut t = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t0 = Instant::now();
        f();
        t.push(t0.elapsed().as_secs_f64());
    }
    t.iter().sum::<f64>() / t.len() as f64
}

/// Measure a closure that returns `Duration` — the closure is responsible for
/// running setup BEFORE starting the timer and returning only the timed portion.
/// This sidesteps the double-`&mut` borrow problem that two separate closures
/// would cause when both capture the same mutable resource.
///
/// Pattern:
/// ```rust
/// measure_timed(|| { reset(&mut db); let t0 = Instant::now(); work(&mut db); t0.elapsed() })
/// ```
fn measure_timed<F: FnMut() -> std::time::Duration>(mut f: F) -> f64 {
    for _ in 0..WARMUP {
        f();
    }
    let mut t = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        t.push(f().as_secs_f64());
    }
    t.iter().sum::<f64>() / t.len() as f64
}

// ── Output ────────────────────────────────────────────────────────────────────

fn out(scenario: &str, n_rows: usize, mean_s: f64, note: &str) {
    let ops = if mean_s > 0.0 {
        (n_rows as f64 / mean_s) as u64
    } else {
        0
    };
    println!(
        r#"{{"engine":"AxiomDB","scenario":"{scenario}","rows":{n_rows},"mean_ms":{mean_ms:.1},"ops_per_s":{ops},"note":"{note}"}}"#,
        mean_ms = mean_s * 1000.0,
    );
}

// ── Scenarios ─────────────────────────────────────────────────────────────────

fn run_scenario(scenario: &str, n_rows: usize, data_dir: &Path) {
    let mut db = Db::open(data_dir);
    let inserts = gen_inserts(n_rows);
    let ac_n = n_rows.min(300);
    let ac = gen_inserts(ac_n);

    match scenario {
        "insert_batch" => {
            // reset (DROP+CREATE) outside timing — measures INSERT only (fair comparison)
            let mean = measure_timed(|| {
                reset(&mut db);
                let t0 = Instant::now();
                insert_batch_pure(&mut db, &inserts);
                t0.elapsed()
            });
            out(scenario, n_rows, mean, "reset outside timing");
        }

        "crud_flow" => {
            // Full cycle: INSERT → SELECT * → DELETE, measured separately per phase.
            // Reset (DROP+CREATE) is outside timing for each iteration.
            let mut ins_t = Vec::with_capacity(RUNS);
            let mut sel_t = Vec::with_capacity(RUNS);
            let mut del_t = Vec::with_capacity(RUNS);

            for i in 0..(WARMUP + RUNS) {
                reset(&mut db);

                // INSERT
                let t0 = Instant::now();
                insert_batch_pure(&mut db, &inserts);
                let t_ins = t0.elapsed().as_secs_f64();

                // SELECT *
                let t0 = Instant::now();
                db.sql_count("SELECT * FROM bench_users");
                let t_sel = t0.elapsed().as_secs_f64();

                // DELETE all rows
                let t0 = Instant::now();
                db.sql("DELETE FROM bench_users");
                let t_del = t0.elapsed().as_secs_f64();

                if i >= WARMUP {
                    ins_t.push(t_ins);
                    sel_t.push(t_sel);
                    del_t.push(t_del);
                }
            }

            let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
            out("crud_flow/insert", n_rows, mean(&ins_t), "");
            out("crud_flow/select", n_rows, mean(&sel_t), "full scan");
            out(
                "crud_flow/delete",
                n_rows,
                mean(&del_t),
                "full scan — index in Phase 5",
            );
        }

        "insert_autocommit" => {
            let mean = measure_timed(|| {
                reset(&mut db);
                let t0 = Instant::now();
                for sql in &ac {
                    db.sql(sql);
                }
                t0.elapsed()
            });
            out(scenario, ac_n, mean, "reset outside timing");
        }

        _ => {
            // Pre-load data for read benchmarks
            load_batch(&mut db, &inserts);
            let step = (n_rows.max(100) / 100).max(1);
            let start = n_rows / 4;
            let end = start + n_rows / 10;

            let (mean, n_ops, note) = match scenario {
                "full_scan" => (
                    measure(|| {
                        db.sql_count("SELECT * FROM bench_users");
                    }),
                    n_rows,
                    "",
                ),
                "select_where" => (
                    measure(|| {
                        db.sql_count("SELECT * FROM bench_users WHERE active = TRUE");
                    }),
                    n_rows / 2,
                    "",
                ),
                "point_lookup" => (
                    measure(|| {
                        for i in (1..=n_rows).step_by(step).take(100) {
                            db.sql_count(&format!("SELECT * FROM bench_users WHERE id = {i}"));
                        }
                    }),
                    100,
                    "full scan — index scan in Phase 5",
                ),
                "range_scan" => (
                    measure(|| {
                        db.sql_count(&format!(
                            "SELECT * FROM bench_users WHERE id >= {start} AND id < {end}"
                        ));
                    }),
                    n_rows / 10,
                    "full scan — index scan in Phase 5",
                ),
                "count_star" => (
                    measure(|| {
                        db.sql("SELECT COUNT(*) FROM bench_users");
                    }),
                    1,
                    "full scan — index in Phase 5",
                ),
                "group_by" => (
                    measure(|| {
                        db.sql("SELECT age, COUNT(*) FROM bench_users GROUP BY age");
                    }),
                    1,
                    "",
                ),
                other => {
                    eprintln!("unknown scenario: {other}");
                    std::process::exit(1);
                }
            };

            out(scenario, n_ops, mean, note);
        }
    }
}

// ── SQLite wrapper ────────────────────────────────────────────────────────────

struct SqliteDb {
    conn: Connection,
}

impl SqliteDb {
    fn open_file(path: &Path) -> Self {
        let conn = Connection::open(path).expect("sqlite open");
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")
            .expect("sqlite pragma");
        Self { conn }
    }

    fn open_memory() -> Self {
        let conn = Connection::open_in_memory().expect("sqlite memory");
        conn.execute_batch("PRAGMA journal_mode=WAL;")
            .expect("sqlite pragma");
        Self { conn }
    }

    fn sql(&self, q: &str) {
        self.conn
            .execute_batch(q)
            .unwrap_or_else(|e| panic!("sqlite({q}): {e}"));
    }

    fn sql_count(&self, q: &str) -> usize {
        let mut stmt = self.conn.prepare_cached(q).expect("prepare");
        let mut rows = stmt.query([]).expect("query");
        let mut n = 0usize;
        while rows.next().expect("next").is_some() {
            n += 1;
        }
        n
    }

    fn reset(&self) {
        self.sql("DROP TABLE IF EXISTS bench_users");
        self.sql(
            "CREATE TABLE bench_users (
                id     INTEGER NOT NULL PRIMARY KEY,
                name   TEXT    NOT NULL,
                age    INTEGER NOT NULL,
                active INTEGER NOT NULL,
                score  REAL    NOT NULL,
                email  TEXT    NOT NULL
            )",
        );
    }
}

// ── SQLite scenarios ──────────────────────────────────────────────────────────

fn sqlite_insert_batch(db: &SqliteDb, inserts: &[String]) {
    db.sql("BEGIN");
    for sql in inserts {
        db.sql(sql);
    }
    db.sql("COMMIT");
}

fn sqlite_load(db: &SqliteDb, inserts: &[String]) {
    db.reset();
    sqlite_insert_batch(db, inserts);
}

fn gen_sqlite_inserts(n: usize) -> Vec<String> {
    (1..=n)
        .map(|i| {
            format!(
                "INSERT INTO bench_users VALUES ({i}, 'user_{i:06}', {age}, {active}, {score:.1}, 'u{i}@b.local')",
                age    = 18 + (i % 62),
                active = if i % 2 == 0 { 1 } else { 0 },
                score  = 100.0 + (i % 1000) as f64 * 0.1,
            )
        })
        .collect()
}

fn run_sqlite_scenario(scenario: &str, n_rows: usize, db: &SqliteDb) -> f64 {
    let inserts = gen_sqlite_inserts(n_rows);
    let ac_n = n_rows.min(300);
    let ac_inserts = gen_sqlite_inserts(ac_n);
    let step = (n_rows.max(100) / 100).max(1);
    let start = n_rows / 4;
    let end = start + n_rows / 10;

    match scenario {
        "insert_batch" => measure_timed(|| {
            db.reset();
            let t0 = Instant::now();
            sqlite_insert_batch(db, &inserts);
            t0.elapsed()
        }),

        "insert_autocommit" => measure_timed(|| {
            db.reset();
            let t0 = Instant::now();
            for sql in &ac_inserts {
                db.sql(&format!("BEGIN; {sql}; COMMIT"));
            }
            t0.elapsed()
        }),

        "crud_flow/insert" | "crud_flow/select" | "crud_flow/delete" => {
            // Run full crud_flow and return only the requested phase
            let mut ins_t = Vec::with_capacity(RUNS);
            let mut sel_t = Vec::with_capacity(RUNS);
            let mut del_t = Vec::with_capacity(RUNS);

            for i in 0..(WARMUP + RUNS) {
                db.reset();
                let t0 = Instant::now();
                sqlite_insert_batch(db, &inserts);
                let t_ins = t0.elapsed().as_secs_f64();

                let t0 = Instant::now();
                db.sql_count("SELECT * FROM bench_users");
                let t_sel = t0.elapsed().as_secs_f64();

                let t0 = Instant::now();
                db.sql("DELETE FROM bench_users");
                let t_del = t0.elapsed().as_secs_f64();

                if i >= WARMUP {
                    ins_t.push(t_ins);
                    sel_t.push(t_sel);
                    del_t.push(t_del);
                }
            }
            let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
            match scenario {
                "crud_flow/insert" => mean(&ins_t),
                "crud_flow/select" => mean(&sel_t),
                _ => mean(&del_t),
            }
        }

        "full_scan" => {
            sqlite_load(db, &inserts);
            measure(|| {
                db.sql_count("SELECT * FROM bench_users");
            })
        }

        "select_where" => {
            sqlite_load(db, &inserts);
            measure(|| {
                db.sql_count("SELECT * FROM bench_users WHERE active = 1");
            })
        }

        "point_lookup" => {
            sqlite_load(db, &inserts);
            measure(|| {
                for i in (1..=n_rows).step_by(step).take(100) {
                    db.sql_count(&format!("SELECT * FROM bench_users WHERE id = {i}"));
                }
            })
        }

        "range_scan" => {
            sqlite_load(db, &inserts);
            measure(|| {
                db.sql_count(&format!(
                    "SELECT * FROM bench_users WHERE id >= {start} AND id < {end}"
                ));
            })
        }

        "count_star" => {
            sqlite_load(db, &inserts);
            measure(|| {
                db.sql_count("SELECT COUNT(*) FROM bench_users");
            })
        }

        "group_by" => {
            sqlite_load(db, &inserts);
            measure(|| {
                db.sql_count("SELECT age, COUNT(*) FROM bench_users GROUP BY age");
            })
        }

        other => panic!("unknown sqlite scenario: {other}"),
    }
}

// ── Comparison report ─────────────────────────────────────────────────────────

fn run_compare(n_rows: usize, sqlite_memory: bool) {
    let sqlite_mode = if sqlite_memory {
        "in-memory"
    } else {
        "WAL file"
    };
    println!("\n╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║  AxiomDB vs SQLite ({sqlite_mode:<10}) — {n_rows} rows per scenario");
    println!("║  Both engines: same process, same data, no network                         ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝\n");

    let axiomdb_dir = tempfile::TempDir::new().expect("tempdir");
    let sqlite_dir = tempfile::TempDir::new().expect("tempdir");

    let mut ax_db = Db::open(axiomdb_dir.path());

    let sq_db = if sqlite_memory {
        SqliteDb::open_memory()
    } else {
        SqliteDb::open_file(&sqlite_dir.path().join("bench.db"))
    };

    let scenarios: &[(&str, usize)] = &[
        ("insert_batch", n_rows),
        ("insert_autocommit", n_rows.min(300)),
        ("crud_flow/insert", n_rows),
        ("crud_flow/select", n_rows),
        ("crud_flow/delete", n_rows),
        ("full_scan", n_rows),
        ("select_where", n_rows),
        ("point_lookup", 100),
        ("range_scan", n_rows / 10),
        ("count_star", 1),
        ("group_by", 1),
    ];

    // Header
    println!(
        "{:<28} {:>14} {:>14} {:>8}  Verdict",
        "Scenario", "AxiomDB", "SQLite", "Ratio"
    );
    println!("{}", "─".repeat(78));

    let mut wins = 0usize;
    let mut total = 0usize;

    for &(scenario, n_ops) in scenarios {
        // AxiomDB
        let ax_s = run_scenario_timed(scenario, n_rows, axiomdb_dir.path(), &mut ax_db);
        // SQLite
        let sq_s = run_sqlite_scenario(scenario, n_rows, &sq_db);

        let ratio = ax_s / sq_s.max(1e-12);
        let verdict = if ratio <= 1.0 {
            "✅ faster"
        } else {
            &format!("⚠️  {ratio:.1}x slower")
        };
        if ratio <= 1.0 {
            wins += 1;
        }
        total += 1;

        let ax_ops = fmt_ops_s(n_ops, ax_s);
        let sq_ops = fmt_ops_s(n_ops, sq_s);

        println!(
            "{:<28} {:>14} {:>14} {:>7.2}x  {}",
            scenario, ax_ops, sq_ops, ratio, verdict
        );
    }

    println!("{}", "─".repeat(78));
    println!("\nAxiomDB wins: {wins}/{total} scenarios");

    if wins == total {
        println!("🚀 AxiomDB beats SQLite on every scenario");
    } else if wins >= total / 2 {
        println!("⚡ AxiomDB leads on majority — investigate ⚠️ scenarios");
    } else {
        println!("🔍 SQLite leads on majority");
        println!("   Tip: run with --diagnose (on AxiomDB side) to find the bottleneck");
    }
    println!();
}

fn run_scenario_timed(scenario: &str, n_rows: usize, _data_dir: &Path, db: &mut Db) -> f64 {
    let inserts = gen_inserts(n_rows);
    let ac_n = n_rows.min(300);
    let ac = gen_inserts(ac_n);
    let step = (n_rows.max(100) / 100).max(1);
    let start = n_rows / 4;
    let end = start + n_rows / 10;

    match scenario {
        "insert_batch" => measure_timed(|| {
            reset(db);
            let t0 = Instant::now();
            insert_batch_pure(db, &inserts);
            t0.elapsed()
        }),

        "insert_autocommit" => measure_timed(|| {
            reset(db);
            let t0 = Instant::now();
            for sql in &ac {
                db.sql(sql);
            }
            t0.elapsed()
        }),

        "crud_flow/insert" | "crud_flow/select" | "crud_flow/delete" => {
            let mut ins_t = Vec::with_capacity(RUNS);
            let mut sel_t = Vec::with_capacity(RUNS);
            let mut del_t = Vec::with_capacity(RUNS);

            for i in 0..(WARMUP + RUNS) {
                reset(db);
                let t0 = Instant::now();
                insert_batch_pure(db, &inserts);
                let t_ins = t0.elapsed().as_secs_f64();

                let t0 = Instant::now();
                db.sql_count("SELECT * FROM bench_users");
                let t_sel = t0.elapsed().as_secs_f64();

                let t0 = Instant::now();
                db.sql("DELETE FROM bench_users");
                let t_del = t0.elapsed().as_secs_f64();

                if i >= WARMUP {
                    ins_t.push(t_ins);
                    sel_t.push(t_sel);
                    del_t.push(t_del);
                }
            }
            let mean = |v: &[f64]| v.iter().sum::<f64>() / v.len() as f64;
            match scenario {
                "crud_flow/insert" => mean(&ins_t),
                "crud_flow/select" => mean(&sel_t),
                _ => mean(&del_t),
            }
        }

        "full_scan" => {
            load_batch(db, &inserts);
            measure(|| {
                db.sql_count("SELECT * FROM bench_users");
            })
        }

        "select_where" => {
            load_batch(db, &inserts);
            measure(|| {
                db.sql_count("SELECT * FROM bench_users WHERE active = TRUE");
            })
        }

        "point_lookup" => {
            load_batch(db, &inserts);
            measure(|| {
                for i in (1..=n_rows).step_by(step).take(100) {
                    db.sql_count(&format!("SELECT * FROM bench_users WHERE id = {i}"));
                }
            })
        }

        "range_scan" => {
            load_batch(db, &inserts);
            measure(|| {
                db.sql_count(&format!(
                    "SELECT * FROM bench_users WHERE id >= {start} AND id < {end}"
                ));
            })
        }

        "count_star" => {
            load_batch(db, &inserts);
            measure(|| {
                db.sql("SELECT COUNT(*) FROM bench_users");
            })
        }

        "group_by" => {
            load_batch(db, &inserts);
            measure(|| {
                db.sql("SELECT age, COUNT(*) FROM bench_users GROUP BY age");
            })
        }

        other => {
            eprintln!("unknown scenario: {other}");
            std::process::exit(1);
        }
    }
}

fn fmt_ops_s(n: usize, s: f64) -> String {
    if s <= 0.0 {
        return "—".to_string();
    }
    let ops = n as f64 / s;
    if ops >= 1_000_000.0 {
        format!("{:.2}M ops/s", ops / 1_000_000.0)
    } else if ops >= 1_000.0 {
        format!("{:.1}K ops/s", ops / 1_000.0)
    } else {
        format!("{:.1} ops/s", ops)
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let n_rows: usize = args
        .iter()
        .skip_while(|a| *a != "--rows")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    // --compare mode: AxiomDB vs SQLite side-by-side
    if args.contains(&"--compare".to_string()) {
        let sqlite_memory = args.contains(&"--sqlite-memory".to_string());
        run_compare(n_rows, sqlite_memory);
        return;
    }

    let scenario = args
        .iter()
        .skip_while(|a| *a != "--scenario")
        .nth(1)
        .expect("--scenario <name> required")
        .as_str()
        .to_owned();

    // Use /data inside container (or temp dir when testing locally)
    let data_dir = if Path::new("/data").exists() {
        std::path::PathBuf::from("/data")
    } else {
        std::env::temp_dir().join("axiomdb_bench")
    };
    std::fs::create_dir_all(&data_dir).unwrap();

    if args.contains(&"--diagnose".to_string()) {
        diagnose(&data_dir, n_rows);
    } else {
        run_scenario(&scenario, n_rows, &data_dir);
    }
}

// ── Diagnostic: timing breakdown per phase ────────────────────────────────────

fn diagnose(data_dir: &Path, n_rows: usize) {
    let mut db = Db::open(data_dir);
    let inserts = gen_inserts(n_rows);
    load_batch(&mut db, &inserts);

    let iters = 200usize;
    let q_scan = "SELECT * FROM bench_users";
    let q_where = "SELECT * FROM bench_users WHERE active = TRUE";
    let q_count = "SELECT COUNT(*) FROM bench_users";
    let q_group = "SELECT age, COUNT(*) FROM bench_users GROUP BY age";

    // ── 1. Parse overhead ─────────────────────────────────────────────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        parse(q_scan, None).unwrap();
    }
    let parse_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── 2. Analyze overhead (catalog lookup) ──────────────────────────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        let stmt = parse(q_scan, None).unwrap();
        let snap = db
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| db.txn.snapshot());
        analyze(stmt, &db.storage, snap).unwrap();
    }
    let analyze_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── 3. Execute — full scan ────────────────────────────────────────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        db.sql_count(q_scan);
    }
    let scan_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── 4. Execute — scan with WHERE filter ──────────────────────────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        db.sql_count(q_where);
    }
    let where_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── 5. Execute — COUNT(*) ─────────────────────────────────────────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        db.sql(q_count);
    }
    let count_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── 6. Execute — GROUP BY ─────────────────────────────────────────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        db.sql(q_group);
    }
    let group_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── Output ────────────────────────────────────────────────────────────────
    let exec_ns = scan_ns.saturating_sub(analyze_ns);
    let analyze_overhead_ns = analyze_ns.saturating_sub(parse_ns);

    fn fmt_us(ns: usize) -> String {
        format!("{:.1} µs", ns as f64 / 1000.0)
    }
    fn pct(part: usize, total: usize) -> String {
        if total == 0 {
            return "  —".to_string();
        }
        format!("{:3.0}%", part as f64 / total as f64 * 100.0)
    }

    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════╗");
    eprintln!(
        "║  AxiomDB Executor Profiling — {} rows, {} iterations",
        n_rows, iters
    );
    eprintln!("╠══════════════════════════════════════════════════════════╣");
    eprintln!("║  Phase breakdown per query call:                        ║");
    eprintln!("║                                                          ║");
    eprintln!(
        "║  parse()         {:>10}                            ║",
        fmt_us(parse_ns)
    );
    eprintln!(
        "║  analyze()       {:>10}  ({} of scan total)      ║",
        fmt_us(analyze_overhead_ns),
        pct(analyze_overhead_ns, scan_ns)
    );
    eprintln!(
        "║  execute-scan    {:>10}  ({} of scan total)      ║",
        fmt_us(exec_ns),
        pct(exec_ns, scan_ns)
    );
    eprintln!("║  ─────────────────────────────────────────────────────  ║");
    eprintln!(
        "║  full_scan total {:>10}                            ║",
        fmt_us(scan_ns)
    );
    eprintln!("║                                                          ║");
    eprintln!(
        "║  SELECT WHERE    {:>10}  (+{} vs scan)           ║",
        fmt_us(where_ns),
        fmt_us(where_ns.saturating_sub(scan_ns))
    );
    eprintln!(
        "║  COUNT(*)        {:>10}                            ║",
        fmt_us(count_ns)
    );
    eprintln!(
        "║  GROUP BY        {:>10}                            ║",
        fmt_us(group_ns)
    );
    eprintln!("║                                                          ║");
    eprintln!(
        "║  Heap scan rate: {:.0} rows/s                          ║",
        n_rows as f64 / (exec_ns as f64 / 1e9)
    );
    eprintln!("╠══════════════════════════════════════════════════════════╣");
    eprintln!("║  Verdict:                                                ║");
    if analyze_overhead_ns > exec_ns {
        eprintln!("║  ⚠️  ANALYZE > EXECUTE — catalog lookup is the bottleneck║");
        eprintln!("║     Fix: schema cache in analyzer (avoids heap scan)    ║");
    } else {
        eprintln!("║  ✅ EXECUTE dominates — bottleneck is the heap scan     ║");
        eprintln!("║     Fix: Phase 5 index scan + vectorized execution       ║");
    }
    eprintln!("╚══════════════════════════════════════════════════════════╝");
    eprintln!();
}
