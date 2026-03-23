//! AxiomDB end-to-end comparison benchmark.
//!
//! Runs the same workloads as benches/comparison/bench_runner.py but using
//! AxiomDB's executor directly (no network layer yet — Phase 8).
//!
//! Storage: MmapStorage (real files on disk)
//! WAL:     enabled with fsync=true (same durability as MySQL/PG in the Python bench)
//! Network: none — direct function calls
//!
//! Usage:
//!   cargo run --release -p axiomdb-bench-comparison -- --rows 10000
//!
//! The output format matches the Python bench_runner.py table so results
//! can be placed side by side.

use std::path::Path;
use std::time::Instant;

use axiomdb_catalog::CatalogBootstrap;
use axiomdb_sql::{analyze, execute, parse};
use axiomdb_storage::MmapStorage;
use axiomdb_wal::TxnManager;

const WARMUP: usize = 2;
const RUNS: usize = 5;

// ── Engine setup ──────────────────────────────────────────────────────────────

struct Db {
    storage: MmapStorage,
    txn: TxnManager,
}

impl Db {
    fn create(dir: &Path) -> Self {
        let db_path = dir.join("bench.db");
        let wal_path = dir.join("bench.wal");
        let mut storage = MmapStorage::create(&db_path).expect("create storage");
        CatalogBootstrap::init(&mut storage).expect("bootstrap catalog");
        let txn = TxnManager::create(&wal_path).expect("create WAL");
        Self { storage, txn }
    }

    fn sql(&mut self, query: &str) -> axiomdb_sql::QueryResult {
        let stmt = parse(query, None).expect(query);
        let snap = self
            .txn
            .active_snapshot()
            .unwrap_or_else(|_| self.txn.snapshot());
        let analyzed = analyze(stmt, &mut self.storage, snap).expect(query);
        execute(analyzed, &mut self.storage, &mut self.txn).expect(query)
    }

    fn sql_rows(&mut self, query: &str) -> usize {
        match self.sql(query) {
            axiomdb_sql::QueryResult::Rows { rows, .. } => rows.len(),
            _ => 0,
        }
    }
}

// ── Data generation ───────────────────────────────────────────────────────────

fn gen_insert(n: usize) -> Vec<String> {
    (1..=n)
        .map(|i| {
            let active = if i % 2 == 0 { "TRUE" } else { "FALSE" };
            let score  = 100.0 + (i % 1000) as f64 * 0.1;
            format!(
                "INSERT INTO bench_users VALUES ({i}, 'user_{i:06}', {age}, {active}, {score:.1}, 'u{i}@b.local')",
                age = 18 + (i % 62),
            )
        })
        .collect()
}

// ── Measurement ───────────────────────────────────────────────────────────────

fn measure<F: FnMut()>(mut f: F) -> (f64, f64) {
    // warmup
    for _ in 0..WARMUP {
        f();
    }
    // measure
    let mut times = Vec::with_capacity(RUNS);
    for _ in 0..RUNS {
        let t = Instant::now();
        f();
        times.push(t.elapsed().as_secs_f64());
    }
    let mean = times.iter().sum::<f64>() / times.len() as f64;
    let var = times.iter().map(|t| (t - mean).powi(2)).sum::<f64>() / times.len() as f64;
    (mean, var.sqrt())
}

// ── Workloads ─────────────────────────────────────────────────────────────────

fn setup_schema(db: &mut Db) {
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

fn truncate(db: &mut Db) {
    db.sql("DELETE FROM bench_users");
}

fn load_batch(db: &mut Db, inserts: &[String]) {
    truncate(db);
    db.sql("BEGIN");
    for sql in inserts {
        db.sql(sql);
    }
    db.sql("COMMIT");
}

// ── Printer ───────────────────────────────────────────────────────────────────

const COL: usize = 26;

fn print_header() {
    let label = "AxiomDB (no network)";
    let sep = "=".repeat(40 + COL + 2);
    println!("{sep}");
    println!("  {:<38}  {:^width$}", "Benchmark", label, width = COL);
    println!("{}", "-".repeat(40 + COL + 2));
}

fn print_row(name: &str, mean_s: f64, n_ops: usize) {
    let ms = mean_s * 1000.0;
    let ops = if mean_s > 0.0 {
        n_ops as f64 / mean_s
    } else {
        0.0
    };
    let ops_str = format_ops(ops as u64);
    let cell = format!("{ms:6.0} ms  {ops_str:>10}/s");
    println!("  {name:<38}  {cell:^width$}", width = COL);
}

fn format_ops(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    for (i, c) in s.chars().rev().enumerate() {
        if i > 0 && i % 3 == 0 {
            result.push(',');
        }
        result.push(c);
    }
    result.chars().rev().collect()
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let n_rows: usize = std::env::args()
        .skip_while(|a| a != "--rows")
        .nth(1)
        .and_then(|v| v.parse().ok())
        .unwrap_or(10_000);

    let ac_rows = n_rows.min(300);

    let dir = tempfile::tempdir().expect("tempdir");
    let mut db = Db::create(dir.path());

    println!("\nAxiomDB end-to-end benchmark");
    println!(
        "  Storage: MmapStorage (real disk I/O at {})",
        dir.path().display()
    );
    println!("  WAL:     enabled (fsync=true)");
    println!("  Network: none (direct function call — Phase 8 adds this)");
    println!("  Rows: {n_rows}  |  {RUNS} runs + {WARMUP} warmup\n");

    let inserts = gen_insert(n_rows);
    let ac_inserts = gen_insert(ac_rows);

    print_header();

    // ── INSERT batch ──────────────────────────────────────────────────────────
    println!("\n  INSERT (batch — 1 txn)");
    setup_schema(&mut db);
    let (mean, _) = measure(|| load_batch(&mut db, &inserts));
    print_row(&format!("insert_batch_{}", n_rows / 1000), mean, n_rows);

    // ── INSERT autocommit ─────────────────────────────────────────────────────
    println!("\n  INSERT (autocommit — 1 txn/row, {ac_rows} rows)");
    setup_schema(&mut db);
    let (mean, _) = measure(|| {
        truncate(&mut db);
        for sql in &ac_inserts {
            db.sql(sql);
        }
    });
    print_row("insert_autocommit_300", mean, ac_rows);

    // ── Reload for SELECT tests ───────────────────────────────────────────────
    setup_schema(&mut db);
    load_batch(&mut db, &inserts);

    // ── SELECT / SCAN ─────────────────────────────────────────────────────────
    println!("\n  SELECT / SCAN  ({n_rows} rows loaded)");

    let (mean, _) = measure(|| {
        db.sql_rows("SELECT * FROM bench_users");
    });
    print_row("select_* full scan", mean, n_rows);

    let (mean, _) = measure(|| {
        db.sql_rows("SELECT * FROM bench_users WHERE active = TRUE");
    });
    print_row("select_where active=1 (~50%)", mean, n_rows / 2);

    // Point lookup: 100 individual queries
    let step = n_rows.max(100) / 100;
    let (mean, _) = measure(|| {
        for i in (1..=n_rows).step_by(step).take(100) {
            db.sql_rows(&format!("SELECT * FROM bench_users WHERE id = {i}"));
        }
    });
    print_row("point_lookup PK × 100", mean, 100);

    // Range scan: 10% of rows
    let start = n_rows / 4;
    let end = start + n_rows / 10;
    let (mean, _) = measure(|| {
        db.sql_rows(&format!(
            "SELECT * FROM bench_users WHERE id >= {start} AND id < {end}"
        ));
    });
    print_row(
        &format!("range_scan 10% ({} rows)", n_rows / 10),
        mean,
        n_rows / 10,
    );

    // ── AGGREGATION ───────────────────────────────────────────────────────────
    println!("\n  AGGREGATION");

    let (mean, _) = measure(|| {
        db.sql("SELECT COUNT(*) FROM bench_users");
    });
    print_row("count(*)", mean, 1);

    let (mean, _) = measure(|| {
        db.sql("SELECT age, COUNT(*) FROM bench_users GROUP BY age");
    });
    print_row("group by age + count(*)", mean, 1);

    println!("{}", "=".repeat(40 + COL + 2));
    println!(
        "\nNote: no network overhead — add ~0.1-0.5ms/query when Phase 8 wire protocol lands."
    );
    println!("Compare INSERT ops/s with MySQL/PG after adding WAL flush cost (~same).\n");
}
