//! AxiomDB internal benchmark — runs INSIDE the AxiomDB container.
//! Uses real MmapStorage + WAL (no network overhead).
//! Outputs one JSON line per scenario to stdout.
//!
//! Usage (from host via docker exec):
//!   docker exec axiomdb_bench_axiomdb /bench/axiomdb_bench \
//!     --scenario insert_batch --rows 10000

use std::path::Path;
use std::time::Instant;

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_sql::{analyze, execute, parse, QueryResult};
use axiomdb_storage::MmapStorage;
use axiomdb_wal::TxnManager;

const WARMUP: usize = 2;
const RUNS: usize = 5;

// ── Engine ────────────────────────────────────────────────────────────────────

struct Db {
    storage: MmapStorage,
    txn: TxnManager,
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

        Self { storage, txn }
    }

    fn sql(&mut self, q: &str) -> QueryResult {
        let stmt = parse(q, None).unwrap_or_else(|e| panic!("parse({q}): {e}"));
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed =
            analyze(stmt, &mut self.storage, snap).unwrap_or_else(|e| panic!("analyze({q}): {e}"));
        execute(analyzed, &mut self.storage, &mut self.txn)
            .unwrap_or_else(|e| panic!("execute({q}): {e}"))
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

fn load_batch(db: &mut Db, inserts: &[String]) {
    reset(db);
    db.sql("BEGIN");
    for sql in inserts {
        db.sql(sql);
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

// ── Output ────────────────────────────────────────────────────────────────────

fn out(scenario: &str, n_rows: usize, mean_s: f64, note: &str) {
    let ops = if mean_s > 0.0 {
        (n_rows as f64 / mean_s) as u64
    } else {
        0
    };
    println!(
        "{}",
        format!(
            r#"{{"engine":"AxiomDB","scenario":"{scenario}","rows":{n_rows},"mean_ms":{mean_ms:.1},"ops_per_s":{ops},"note":"{note}"}}"#,
            mean_ms = mean_s * 1000.0,
        )
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
            let mean = measure(|| load_batch(&mut db, &inserts));
            out(scenario, n_rows, mean, "");
        }

        "insert_autocommit" => {
            let mean = measure(|| {
                reset(&mut db);
                for sql in &ac {
                    db.sql(sql);
                }
            });
            out(scenario, ac_n, mean, "");
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

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();

    let scenario = args
        .iter()
        .skip_while(|a| *a != "--scenario")
        .nth(1)
        .expect("--scenario <name> required")
        .as_str()
        .to_owned();

    let n_rows: usize = args
        .iter()
        .skip_while(|a| *a != "--rows")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    // Use /data inside container (or temp dir when testing locally)
    let data_dir = if Path::new("/data").exists() {
        std::path::PathBuf::from("/data")
    } else {
        std::env::temp_dir().join("axiomdb_bench")
    };
    std::fs::create_dir_all(&data_dir).unwrap();

    run_scenario(&scenario, n_rows, &data_dir);
}

// ── Diagnostic: timing breakdown per phase ────────────────────────────────────

#[allow(dead_code)]
fn diagnose(data_dir: &Path) {
    let mut db = Db::open(data_dir);
    let inserts = gen_inserts(1000);
    load_batch(&mut db, &inserts);

    let q = "SELECT * FROM bench_users";

    // 1. Parse only
    let t0 = Instant::now();
    for _ in 0..100 {
        parse(q, None).unwrap();
    }
    let parse_us = t0.elapsed().as_micros() / 100;

    // 2. Parse + analyze
    let t0 = Instant::now();
    for _ in 0..100 {
        let stmt = parse(q, None).unwrap();
        let snap = db
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| db.txn.snapshot());
        analyze(stmt, &mut db.storage, snap).unwrap();
    }
    let analyze_us = t0.elapsed().as_micros() / 100;

    // 3. Full execute
    let t0 = Instant::now();
    for _ in 0..100 {
        db.sql_count(q);
    }
    let execute_us = t0.elapsed().as_micros() / 100;

    eprintln!("=== DIAGNOSE (1K rows) ===");
    eprintln!("  parse only:        {:>6} µs", parse_us);
    eprintln!(
        "  parse + analyze:   {:>6} µs  (analyze overhead: {} µs)",
        analyze_us,
        analyze_us - parse_us
    );
    eprintln!(
        "  full execute:      {:>6} µs  (execute overhead: {} µs)",
        execute_us,
        execute_us - analyze_us
    );
    eprintln!(
        "  analyze % of total: {:.0}%",
        (analyze_us - parse_us) as f64 / execute_us as f64 * 100.0
    );
}
