//! AxiomDB internal benchmark — in-process, no network overhead.
//! Uses axiomdb-embedded::Db which keeps SchemaCache + SessionContext
//! alive across queries — same behavior as the wire server.
//! Outputs one JSON line per scenario to stdout.
//!
//! Comparison mode (AxiomDB vs SQLite, same process, no network):
//!   cargo run -p axiomdb-bench-comparison --release -- --compare --rows 5000
//!   cargo run -p axiomdb-bench-comparison --release -- --compare --rows 5000 --sqlite-memory

use std::path::Path;
use std::time::Instant;

use axiomdb_embedded::Db;
use axiomdb_sql::QueryResult;
use rusqlite::Connection;

const WARMUP: usize = 2;
const RUNS: usize = 5;

// ── Engine helpers ────────────────────────────────────────────────────────────

fn db_open(dir: &Path) -> Db {
    let mut db = Db::open(dir.join("bench.db")).expect("open db");
    // Attack 6: match SQLite's PRAGMA synchronous=NORMAL for an
    // apples-to-apples comparison on the insert_autocommit scenario.
    // SqliteDb::open_file above sets the same level (line 464).
    // Without this, AxiomDB pays fsync-per-commit while SQLite does not,
    // and the bench is fundamentally unfair on durability cost.
    db.run("SET synchronous = 'NORMAL'")
        .expect("set synchronous=NORMAL");
    db
}

fn db_sql(db: &mut Db, q: &str) -> QueryResult {
    db.run(q).unwrap_or_else(|e| panic!("sql({q}): {e}"))
}

fn db_sql_count(db: &mut Db, q: &str) -> usize {
    db.query(q)
        .unwrap_or_else(|e| panic!("query({q}): {e}"))
        .len()
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
    db_sql(db, "DROP TABLE IF EXISTS bench_users");
    db_sql(
        db,
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
    insert_batch_pure(db, inserts);
}

fn insert_batch_pure(db: &mut Db, inserts: &[String]) {
    db_sql(db, "BEGIN");
    for sql in inserts {
        db_sql(db, sql);
    }
    db_sql(db, "COMMIT");
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
    let mut db = db_open(data_dir);
    let inserts = gen_inserts(n_rows);
    let ac_n = n_rows.min(300);
    let ac = gen_inserts(ac_n);

    match scenario {
        "insert_batch" => {
            let mean = measure_timed(|| {
                reset(&mut db);
                let t0 = Instant::now();
                insert_batch_pure(&mut db, &inserts);
                t0.elapsed()
            });
            out(scenario, n_rows, mean, "reset outside timing");
        }

        "crud_flow" => {
            let mut ins_t = Vec::with_capacity(RUNS);
            let mut sel_t = Vec::with_capacity(RUNS);
            let mut del_t = Vec::with_capacity(RUNS);

            for i in 0..(WARMUP + RUNS) {
                reset(&mut db);

                let t0 = Instant::now();
                insert_batch_pure(&mut db, &inserts);
                let t_ins = t0.elapsed().as_secs_f64();

                let t0 = Instant::now();
                db_sql_count(&mut db, "SELECT * FROM bench_users");
                let t_sel = t0.elapsed().as_secs_f64();

                let t0 = Instant::now();
                db_sql(&mut db, "DELETE FROM bench_users");
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
            out("crud_flow/delete", n_rows, mean(&del_t), "");
        }

        "insert_autocommit" => {
            let mean = measure_timed(|| {
                reset(&mut db);
                let t0 = Instant::now();
                for sql in &ac {
                    db_sql(&mut db, sql);
                }
                t0.elapsed()
            });
            out(scenario, ac_n, mean, "reset outside timing");
        }

        "insert_appender" => {
            // Attack 7 v1.1: Appender on the canonical clustered
            // bench_users table (PRIMARY KEY id). Goes through the
            // clustered B-Tree + all secondary indexes — same write
            // path SQL INSERT uses for this schema.
            use axiomdb_types::Value;
            let mean = measure_timed(|| {
                reset(&mut db);
                let t0 = Instant::now();
                let mut app = db.appender("bench_users").unwrap();
                for i in 1..=ac_n {
                    let active = i % 2 == 0;
                    app.append_row_owned(vec![
                        Value::Int(i as i32),
                        Value::Text(format!("user_{i:06}")),
                        Value::Int((18 + (i % 62)) as i32),
                        Value::Bool(active),
                        Value::Real(100.0 + (i % 1000) as f64 * 0.1),
                        Value::Text(format!("u{i}@b.local")),
                    ])
                    .unwrap();
                }
                app.finish().unwrap();
                t0.elapsed()
            });
            out(scenario, ac_n, mean, "appender, clustered (v1.1)");
        }

        "insert_appender_large" => {
            // Attack 13 validation: uncapped rows to exercise multiple
            // auto-flushes (APPENDER_BATCH_FLUSH = 1024). The deferred
            // `update_table_root` saves one catalog write per flush
            // avoided (8.7ms on macOS APFS).
            use axiomdb_types::Value;
            let mean = measure_timed(|| {
                reset(&mut db);
                let t0 = Instant::now();
                let mut app = db.appender("bench_users").unwrap();
                for i in 1..=n_rows {
                    let active = i % 2 == 0;
                    app.append_row_owned(vec![
                        Value::Int(i as i32),
                        Value::Text(format!("user_{i:06}")),
                        Value::Int((18 + (i % 62)) as i32),
                        Value::Bool(active),
                        Value::Real(100.0 + (i % 1000) as f64 * 0.1),
                        Value::Text(format!("u{i}@b.local")),
                    ])
                    .unwrap();
                }
                app.finish().unwrap();
                t0.elapsed()
            });
            out(
                scenario,
                n_rows,
                mean,
                "appender clustered, n_rows uncapped (A13)",
            );
        }

        "create_index" => {
            // Attack 12: CREATE INDEX timing on the canonical 5000-row
            // bench_users table. Uses non-unique idx_email (the
            // bulk-eligible case). UNIQUE indexes fall back to the
            // per-row path.
            let mean = measure_timed(|| {
                reset(&mut db);
                let inserts = gen_inserts(n_rows);
                insert_batch_pure(&mut db, &inserts);
                db_sql(&mut db, "DROP INDEX IF EXISTS idx_email");
                let t0 = Instant::now();
                db_sql(&mut db, "CREATE INDEX idx_email ON bench_users (email)");
                t0.elapsed()
            });
            out(scenario, n_rows, mean, "CREATE INDEX on n_rows-row table");
        }

        "insert_appender_indexed" => {
            // Attack 10 validation: clustered bench_users + a secondary
            // index. The deferred-secondary + catalog-batching wins are
            // ONLY visible on tables WITH secondary indexes — the plain
            // bench_users has none, so insert_appender doesn't expose
            // the gain. Here we add `CREATE INDEX idx_email` and
            // re-measure.
            use axiomdb_types::Value;
            let mean = measure_timed(|| {
                reset(&mut db);
                db_sql(&mut db, "CREATE INDEX idx_email ON bench_users (email)");
                let t0 = Instant::now();
                let mut app = db.appender("bench_users").unwrap();
                for i in 1..=ac_n {
                    let active = i % 2 == 0;
                    app.append_row_owned(vec![
                        Value::Int(i as i32),
                        Value::Text(format!("user_{i:06}")),
                        Value::Int((18 + (i % 62)) as i32),
                        Value::Bool(active),
                        Value::Real(100.0 + (i % 1000) as f64 * 0.1),
                        Value::Text(format!("u{i}@b.local")),
                    ])
                    .unwrap();
                }
                app.finish().unwrap();
                t0.elapsed()
            });
            out(
                scenario,
                ac_n,
                mean,
                "appender, clustered+1 secondary index",
            );
        }

        "insert_appender_heap" => {
            // Attack 7 v1 baseline kept for comparison: Appender on a
            // parallel heap-only table (no PRIMARY KEY). Shows the
            // ceiling of the Appender when the clustered B-Tree split
            // overhead is removed.
            use axiomdb_types::Value;
            let mean = measure_timed(|| {
                db_sql(&mut db, "DROP TABLE IF EXISTS bench_users_heap");
                db_sql(
                    &mut db,
                    "CREATE TABLE bench_users_heap (
                        id     INT  NOT NULL,
                        name   TEXT NOT NULL,
                        age    INT  NOT NULL,
                        active BOOL NOT NULL,
                        score  REAL NOT NULL,
                        email  TEXT NOT NULL
                    )",
                );
                let t0 = Instant::now();
                let mut app = db.appender("bench_users_heap").unwrap();
                for i in 1..=ac_n {
                    let active = i % 2 == 0;
                    app.append_row_owned(vec![
                        Value::Int(i as i32),
                        Value::Text(format!("user_{i:06}")),
                        Value::Int((18 + (i % 62)) as i32),
                        Value::Bool(active),
                        Value::Real(100.0 + (i % 1000) as f64 * 0.1),
                        Value::Text(format!("u{i}@b.local")),
                    ])
                    .unwrap();
                }
                app.finish().unwrap();
                t0.elapsed()
            });
            out(scenario, ac_n, mean, "appender, heap (v1 baseline)");
        }

        "insert_appender_typed" => {
            // Attack 8: Rust typed builder API. Should match v1.1
            // performance — same internal path through append_row_owned.
            let mean = measure_timed(|| {
                db_sql(&mut db, "DROP TABLE IF EXISTS bench_users_heap");
                db_sql(
                    &mut db,
                    "CREATE TABLE bench_users_heap (
                        id     INT  NOT NULL,
                        name   TEXT NOT NULL,
                        age    INT  NOT NULL,
                        active BOOL NOT NULL,
                        score  REAL NOT NULL,
                        email  TEXT NOT NULL
                    )",
                );
                let t0 = Instant::now();
                let mut app = db.appender("bench_users_heap").unwrap();
                for i in 1..=ac_n {
                    app.append_int(i as i32).unwrap();
                    app.append_text(&format!("user_{i:06}")).unwrap();
                    app.append_int((18 + (i % 62)) as i32).unwrap();
                    app.append_bool(i % 2 == 0).unwrap();
                    app.append_real(100.0 + (i % 1000) as f64 * 0.1).unwrap();
                    app.append_text(&format!("u{i}@b.local")).unwrap();
                    app.end_row().unwrap();
                }
                app.finish().unwrap();
                t0.elapsed()
            });
            out(scenario, ac_n, mean, "appender typed builder (Rust, A8)");
        }

        "insert_appender_c" => {
            // Attack 8: through the C FFI surface from Rust. Measures
            // the per-call cost of the unsafe boundary.
            use std::ffi::CString;
            let mean = measure_timed(|| {
                db_sql(&mut db, "DROP TABLE IF EXISTS bench_users_heap");
                db_sql(
                    &mut db,
                    "CREATE TABLE bench_users_heap (
                        id     INT  NOT NULL,
                        name   TEXT NOT NULL,
                        age    INT  NOT NULL,
                        active BOOL NOT NULL,
                        score  REAL NOT NULL,
                        email  TEXT NOT NULL
                    )",
                );
                let t0 = Instant::now();
                unsafe {
                    let db_ptr: *mut axiomdb_embedded::Db = &mut db;
                    let table = CString::new("bench_users_heap").unwrap();
                    let app = axiomdb_embedded::axiomdb_appender_open(db_ptr, table.as_ptr());
                    assert!(!app.is_null());
                    for i in 1..=ac_n {
                        let name = CString::new(format!("user_{i:06}")).unwrap();
                        let email = CString::new(format!("u{i}@b.local")).unwrap();
                        axiomdb_embedded::axiomdb_appender_append_int(app, i as i32);
                        axiomdb_embedded::axiomdb_appender_append_text(app, name.as_ptr());
                        axiomdb_embedded::axiomdb_appender_append_int(app, (18 + (i % 62)) as i32);
                        axiomdb_embedded::axiomdb_appender_append_bool(
                            app,
                            if i % 2 == 0 { 1 } else { 0 },
                        );
                        axiomdb_embedded::axiomdb_appender_append_real(
                            app,
                            100.0 + (i % 1000) as f64 * 0.1,
                        );
                        axiomdb_embedded::axiomdb_appender_append_text(app, email.as_ptr());
                        axiomdb_embedded::axiomdb_appender_end_row(app);
                    }
                    let n = axiomdb_embedded::axiomdb_appender_finish(app);
                    assert!(n >= 0);
                }
                t0.elapsed()
            });
            out(scenario, ac_n, mean, "appender C FFI (Attack 8)");
        }

        _ => {
            load_batch(&mut db, &inserts);
            let step = (n_rows.max(100) / 100).max(1);
            let start = n_rows / 4;
            let end = start + n_rows / 10;

            let (mean, n_ops, note) = match scenario {
                "full_scan" => (
                    measure(|| {
                        db_sql_count(&mut db, "SELECT * FROM bench_users");
                    }),
                    n_rows,
                    "",
                ),
                "select_where" => (
                    measure(|| {
                        db_sql_count(&mut db, "SELECT * FROM bench_users WHERE active = TRUE");
                    }),
                    n_rows / 2,
                    "",
                ),
                "point_lookup" => (
                    measure(|| {
                        for i in (1..=n_rows).step_by(step).take(100) {
                            db_sql_count(
                                &mut db,
                                &format!("SELECT * FROM bench_users WHERE id = {i}"),
                            );
                        }
                    }),
                    100,
                    "",
                ),
                "range_scan" => (
                    measure(|| {
                        db_sql_count(
                            &mut db,
                            &format!(
                                "SELECT * FROM bench_users WHERE id >= {start} AND id < {end}"
                            ),
                        );
                    }),
                    n_rows / 10,
                    "",
                ),
                "count_star" => (
                    measure(|| {
                        db_sql(&mut db, "SELECT COUNT(*) FROM bench_users");
                    }),
                    1,
                    "",
                ),
                "group_by" => (
                    measure(|| {
                        db_sql(
                            &mut db,
                            "SELECT age, COUNT(*) FROM bench_users GROUP BY age",
                        );
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

// ── Diagnostic: INSERT timing breakdown (parse / analyze / execute) ──────────
//
// Recreates the embedded Db components manually so we can wrap each phase
// with its own timer. No production code changes.
//
// Layout of one INSERT call inside `Db::run_inner` (from axiomdb-embedded
// lib.rs:294) is:
//     1. parse_with_sql_mode(...)        ← AST construction
//     2. txn.snapshot()                  ← MVCC snapshot allocation
//     3. analyze_cached(...)             ← name resolution (schema cache hit)
//     4. execute_with_ctx(...)           ← all the real work
//
// All four are timed separately below. The fourth bucket (execute) lumps
// together the row codec, heap insert, WAL append, MVCC bookkeeping, PK
// index update, and autocommit. A follow-up diagnostic can break execute
// down further once we know it dominates.
fn diagnose_insert(data_dir: &Path, n_rows: usize) {
    use axiomdb_catalog::bootstrap::CatalogBootstrap;
    use axiomdb_sql::{
        analyze_cached, bloom::BloomRegistry, execute_with_ctx, parse_with_sql_mode, SchemaCache,
        SessionContext,
    };
    use axiomdb_storage::MmapStorage;
    use axiomdb_wal::TxnManager;

    // Always start from a clean DB so the bench is reproducible.
    let _ = std::fs::remove_dir_all(data_dir);
    std::fs::create_dir_all(data_dir).unwrap();
    let db_path = data_dir.join("diag_insert.db");
    let wal_path = data_dir.join("diag_insert.wal");

    let storage = MmapStorage::create(&db_path).expect("create storage");
    CatalogBootstrap::init(&storage).expect("bootstrap catalog");
    let txn = TxnManager::create(&wal_path).expect("create txn");
    let bloom = BloomRegistry::new();
    let mut schema_cache = SchemaCache::new();
    let mut session = SessionContext::default();
    let sql_mode = session.sql_mode_flags();

    // ── CREATE TABLE (not part of the measurement) ────────────────────────────
    let create_sql = "CREATE TABLE bench_users (\
        id INT NOT NULL, name TEXT NOT NULL, age INT NOT NULL, \
        active BOOL NOT NULL, score REAL NOT NULL, email TEXT NOT NULL, \
        PRIMARY KEY (id))";
    let create_stmt = parse_with_sql_mode(create_sql, None, sql_mode).expect("parse create");
    let create_analyzed = analyze_cached(create_stmt, &storage, txn.snapshot(), &mut schema_cache)
        .expect("analyze create");
    execute_with_ctx(create_analyzed, &storage, &txn, &bloom, &mut session).expect("create");

    // ── BEGIN (one explicit transaction wrapping all inserts) ─────────────────
    let begin_stmt = parse_with_sql_mode("BEGIN", None, sql_mode).unwrap();
    let begin_analyzed =
        analyze_cached(begin_stmt, &storage, txn.snapshot(), &mut schema_cache).unwrap();
    execute_with_ctx(begin_analyzed, &storage, &txn, &bloom, &mut session).unwrap();

    let sqls = gen_inserts(n_rows);

    // ── Warmup so caches are hot for the first measured row ───────────────────
    let first = parse_with_sql_mode(&sqls[0], None, sql_mode).unwrap();
    let first = analyze_cached(first, &storage, txn.snapshot(), &mut schema_cache).unwrap();
    execute_with_ctx(first, &storage, &txn, &bloom, &mut session).unwrap();

    // ── Timed loop — separate timers around each phase ────────────────────────
    let mut t_parse_ns: u128 = 0;
    let mut t_snap_ns: u128 = 0;
    let mut t_analyze_ns: u128 = 0;
    let mut t_execute_ns: u128 = 0;

    let measured_start = 1; // we already executed row 0 above as warmup
    for sql in &sqls[measured_start..] {
        // Phase 1: parse
        let t0 = Instant::now();
        let stmt = parse_with_sql_mode(sql, None, sql_mode).expect("parse insert");
        t_parse_ns += t0.elapsed().as_nanos();

        // Phase 2: snapshot acquisition
        let t0 = Instant::now();
        let snap = if let Some(ref ct) = session.conn_txn {
            txn.active_snapshot(ct)
        } else {
            txn.snapshot()
        };
        t_snap_ns += t0.elapsed().as_nanos();

        // Phase 3: analyze (schema cache hit after the first row)
        let t0 = Instant::now();
        let analyzed = analyze_cached(stmt, &storage, snap, &mut schema_cache).expect("analyze");
        t_analyze_ns += t0.elapsed().as_nanos();

        // Phase 4: execute (everything else — codec, heap, WAL, MVCC, index)
        let t0 = Instant::now();
        execute_with_ctx(analyzed, &storage, &txn, &bloom, &mut session).expect("execute");
        t_execute_ns += t0.elapsed().as_nanos();
    }

    // ── COMMIT (timed separately — captures group fsync) ──────────────────────
    let commit_stmt = parse_with_sql_mode("COMMIT", None, sql_mode).unwrap();
    let commit_analyzed =
        analyze_cached(commit_stmt, &storage, txn.snapshot(), &mut schema_cache).unwrap();
    let t0 = Instant::now();
    execute_with_ctx(commit_analyzed, &storage, &txn, &bloom, &mut session).unwrap();
    let commit_ns = t0.elapsed().as_nanos();

    // ── Report ────────────────────────────────────────────────────────────────
    let measured = (n_rows - measured_start) as u128;
    let per = |total: u128| total / measured;
    let total_per_row = per(t_parse_ns) + per(t_snap_ns) + per(t_analyze_ns) + per(t_execute_ns);

    fn fmt_us(ns: u128) -> String {
        format!("{:.2} µs", ns as f64 / 1000.0)
    }
    fn pct(part: u128, total: u128) -> String {
        if total == 0 {
            "  —".into()
        } else {
            format!("{:5.1}%", part as f64 / total as f64 * 100.0)
        }
    }

    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════════════╗");
    eprintln!(
        "║  AxiomDB INSERT Phase Breakdown — {} rows (warmup discarded)   ",
        n_rows
    );
    eprintln!("║  Configuration: explicit BEGIN/COMMIT, single connection         ║");
    eprintln!("╠══════════════════════════════════════════════════════════════════╣");
    eprintln!("║  Phase                       µs/row    % of per-row total       ║");
    eprintln!("║  ────────────────────────────────────────────────────────────    ║");
    eprintln!(
        "║  parse_with_sql_mode      {:>10}     {}                  ║",
        fmt_us(per(t_parse_ns)),
        pct(per(t_parse_ns), total_per_row)
    );
    eprintln!(
        "║  txn.snapshot()           {:>10}     {}                  ║",
        fmt_us(per(t_snap_ns)),
        pct(per(t_snap_ns), total_per_row)
    );
    eprintln!(
        "║  analyze_cached           {:>10}     {}                  ║",
        fmt_us(per(t_analyze_ns)),
        pct(per(t_analyze_ns), total_per_row)
    );
    eprintln!(
        "║  execute_with_ctx         {:>10}     {}                  ║",
        fmt_us(per(t_execute_ns)),
        pct(per(t_execute_ns), total_per_row)
    );
    eprintln!("║  ────────────────────────────────────────────────────────────    ║");
    eprintln!(
        "║  TOTAL per row            {:>10}                            ║",
        fmt_us(total_per_row)
    );
    eprintln!("║                                                                  ║");
    eprintln!(
        "║  COMMIT (group fsync)     {:>10}  (amortized over {} rows)  ",
        fmt_us(commit_ns),
        n_rows
    );
    eprintln!("║                                                                  ║");
    eprintln!(
        "║  Effective throughput:    {:.0} rows/s                            ",
        1e9 / total_per_row as f64
    );
    eprintln!("╠══════════════════════════════════════════════════════════════════╣");

    // Verdict
    let dominant = [
        ("parse", per(t_parse_ns)),
        ("snapshot", per(t_snap_ns)),
        ("analyze", per(t_analyze_ns)),
        ("execute", per(t_execute_ns)),
    ]
    .iter()
    .max_by_key(|(_, v)| *v)
    .copied()
    .unwrap();
    eprintln!(
        "║  Dominant phase: {} ({:.1}% of per-row time)",
        dominant.0,
        dominant.1 as f64 / total_per_row as f64 * 100.0,
    );
    if dominant.0 == "execute" {
        eprintln!("║  Next step: instrument inside execute_with_ctx to break          ║");
        eprintln!("║  execute down into: row codec, heap insert, WAL, index update.   ║");
    }
    eprintln!("╚══════════════════════════════════════════════════════════════════╝");
    eprintln!();
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
        let ncol = stmt.column_count();
        let mut rows = stmt.query([]).expect("query");
        let mut n = 0usize;
        // Materialize EVERY column per row (owned String for TEXT) so this is
        // apples-to-apples with AxiomDB's `db.query()` -> Vec<Row>, which decodes
        // and allocates all columns. Without this, SQLite's lazy OP_Column never
        // touches the TEXT columns when we only step+count, making the scan
        // comparison unfair (it would measure SQLite-lazy-step vs AxiomDB-eager).
        while let Some(row) = rows.next().expect("next") {
            for i in 0..ncol {
                match row.get_ref(i).expect("get_ref") {
                    rusqlite::types::ValueRef::Text(t) => {
                        let s = std::str::from_utf8(t).expect("utf8").to_string();
                        std::hint::black_box(s);
                    }
                    rusqlite::types::ValueRef::Integer(v) => {
                        std::hint::black_box(v);
                    }
                    rusqlite::types::ValueRef::Real(v) => {
                        std::hint::black_box(v);
                    }
                    rusqlite::types::ValueRef::Blob(b) => {
                        std::hint::black_box(b.len());
                    }
                    rusqlite::types::ValueRef::Null => {}
                }
            }
            n += 1;
        }
        n
    }

    /// Point lookups via ONE parameterized prepared statement, bound per id —
    /// matches AxiomDB's prepared+bind path for a fair engine comparison
    /// (no per-call re-prepare). Materializes every column like `sql_count`.
    fn point_lookups(&self, ids: &[i64]) {
        let mut stmt = self
            .conn
            .prepare("SELECT * FROM bench_users WHERE id = ?")
            .expect("prepare");
        let ncol = stmt.column_count();
        for &id in ids {
            let mut rows = stmt.query([id]).expect("query");
            while let Some(row) = rows.next().expect("next") {
                for i in 0..ncol {
                    match row.get_ref(i).expect("get_ref") {
                        rusqlite::types::ValueRef::Text(t) => {
                            std::hint::black_box(std::str::from_utf8(t).expect("utf8").to_string());
                        }
                        rusqlite::types::ValueRef::Integer(v) => {
                            std::hint::black_box(v);
                        }
                        rusqlite::types::ValueRef::Real(v) => {
                            std::hint::black_box(v);
                        }
                        rusqlite::types::ValueRef::Blob(b) => {
                            std::hint::black_box(b.len());
                        }
                        rusqlite::types::ValueRef::Null => {}
                    }
                }
            }
        }
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

        // Attack 7 comparison: SQLite analog of AxiomDB Appender.
        // Uses prepare_cached + execute with bind params (no parsing
        // per row, no fsync per row in NORMAL mode) — the canonical
        // "fast bind" pattern SQLite users actually use for bulk loads.
        "insert_appender" | "insert_appender_heap" => measure_timed(|| {
            // Same SQLite fast-bind path for both — SQLite doesn't
            // distinguish heap vs clustered (every table is a B-Tree).
            db.reset();
            let n = ac_inserts.len();
            let t0 = Instant::now();
            db.conn.execute_batch("BEGIN").unwrap();
            {
                let mut stmt = db
                    .conn
                    .prepare_cached("INSERT INTO bench_users VALUES (?, ?, ?, ?, ?, ?)")
                    .unwrap();
                for i in 1..=n {
                    let active = if i % 2 == 0 { 1i64 } else { 0 };
                    let age = (18 + (i % 62)) as i64;
                    let score = 100.0 + (i % 1000) as f64 * 0.1;
                    let name = format!("user_{i:06}");
                    let email = format!("u{i}@b.local");
                    stmt.execute(rusqlite::params![i as i64, name, age, active, score, email])
                        .unwrap();
                }
            }
            db.conn.execute_batch("COMMIT").unwrap();
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
            let ids: Vec<i64> = (1..=n_rows)
                .step_by(step)
                .take(100)
                .map(|i| i as i64)
                .collect();
            measure(|| {
                db.point_lookups(&ids);
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

    let mut ax_db = db_open(axiomdb_dir.path());

    let sq_db = if sqlite_memory {
        SqliteDb::open_memory()
    } else {
        SqliteDb::open_file(&sqlite_dir.path().join("bench.db"))
    };

    let scenarios: &[(&str, usize)] = &[
        ("insert_batch", n_rows),
        ("insert_autocommit", n_rows.min(300)),
        ("insert_appender", n_rows.min(300)),
        ("insert_appender_heap", n_rows.min(300)),
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
                db_sql(db, sql);
            }
            t0.elapsed()
        }),

        "insert_appender" => {
            // v1.1: clustered bench_users (PRIMARY KEY id).
            use axiomdb_types::Value;
            measure_timed(|| {
                reset(db);
                let t0 = Instant::now();
                let mut app = db.appender("bench_users").unwrap();
                for i in 1..=ac_n {
                    let active = i % 2 == 0;
                    app.append_row_owned(vec![
                        Value::Int(i as i32),
                        Value::Text(format!("user_{i:06}")),
                        Value::Int((18 + (i % 62)) as i32),
                        Value::Bool(active),
                        Value::Real(100.0 + (i % 1000) as f64 * 0.1),
                        Value::Text(format!("u{i}@b.local")),
                    ])
                    .unwrap();
                }
                app.finish().unwrap();
                t0.elapsed()
            })
        }

        "insert_appender_heap" => {
            // v1 baseline retained: heap-only bench_users_heap.
            use axiomdb_types::Value;
            measure_timed(|| {
                db_sql(db, "DROP TABLE IF EXISTS bench_users_heap");
                db_sql(
                    db,
                    "CREATE TABLE bench_users_heap (
                        id     INT  NOT NULL,
                        name   TEXT NOT NULL,
                        age    INT  NOT NULL,
                        active BOOL NOT NULL,
                        score  REAL NOT NULL,
                        email  TEXT NOT NULL
                    )",
                );
                let t0 = Instant::now();
                let mut app = db.appender("bench_users_heap").unwrap();
                for i in 1..=ac_n {
                    let active = i % 2 == 0;
                    app.append_row_owned(vec![
                        Value::Int(i as i32),
                        Value::Text(format!("user_{i:06}")),
                        Value::Int((18 + (i % 62)) as i32),
                        Value::Bool(active),
                        Value::Real(100.0 + (i % 1000) as f64 * 0.1),
                        Value::Text(format!("u{i}@b.local")),
                    ])
                    .unwrap();
                }
                app.finish().unwrap();
                t0.elapsed()
            })
        }

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
                db_sql_count(db, "SELECT * FROM bench_users");
                let t_sel = t0.elapsed().as_secs_f64();

                let t0 = Instant::now();
                db_sql(db, "DELETE FROM bench_users");
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
                db_sql_count(db, "SELECT * FROM bench_users");
            })
        }

        "select_where" => {
            load_batch(db, &inserts);
            measure(|| {
                db_sql_count(db, "SELECT * FROM bench_users WHERE active = TRUE");
            })
        }

        "point_lookup" => {
            load_batch(db, &inserts);
            // One parameterized prepared statement, bound per id (the OLTP pattern;
            // matches SQLite's prepared+bind below). Measures engine lookup, not parse.
            let stmt = db
                .prepare("SELECT * FROM bench_users WHERE id = ?")
                .expect("prepare point_lookup");
            measure(|| {
                for i in (1..=n_rows).step_by(step).take(100) {
                    stmt.execute(db, &[axiomdb_types::Value::Int(i as i32)])
                        .expect("point_lookup exec");
                }
            })
        }

        "range_scan" => {
            load_batch(db, &inserts);
            measure(|| {
                db_sql_count(
                    db,
                    &format!("SELECT * FROM bench_users WHERE id >= {start} AND id < {end}"),
                );
            })
        }

        "count_star" => {
            load_batch(db, &inserts);
            // Prepared once (matches SQLite's prepare_cached reuse) — measures the
            // engine count path, not per-call parse.
            let stmt = db
                .prepare("SELECT COUNT(*) FROM bench_users")
                .expect("prepare count_star");
            measure(|| {
                stmt.execute(db, &[]).expect("count_star exec");
            })
        }

        "group_by" => {
            load_batch(db, &inserts);
            measure(|| {
                db_sql(db, "SELECT age, COUNT(*) FROM bench_users GROUP BY age");
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

/// Diagnose where a WARM full_scan spends time, and whether it touches mmap.
/// Loads once, warms the buffer pool, then loops the scan while sampling
/// minor/major page faults from /proc/self/stat. ≈0 faults/scan ⇒ the warm
/// scan is served from the buffer pool (no mmap) ⇒ the cost is CPU/memory, not
/// page faults.
fn diagnose_scan(data_dir: &Path, n_rows: usize) {
    fn read_faults() -> (u64, u64) {
        // /proc/self/stat: after the `comm` field (in parens), token[7] = minflt,
        // token[9] = majflt (0-based, counting from the first field after `)`).
        let s = match std::fs::read_to_string("/proc/self/stat") {
            Ok(s) => s,
            Err(_) => return (0, 0),
        };
        let rp = match s.rfind(')') {
            Some(i) => i,
            None => return (0, 0),
        };
        let rest: Vec<&str> = s[rp + 1..].split_whitespace().collect();
        let minflt = rest.get(7).and_then(|x| x.parse().ok()).unwrap_or(0);
        let majflt = rest.get(9).and_then(|x| x.parse().ok()).unwrap_or(0);
        (minflt, majflt)
    }

    let mut db = db_open(data_dir);
    reset(&mut db);
    let inserts = gen_inserts(n_rows);
    load_batch(&mut db, &inserts);

    for _ in 0..3 {
        let _ = db.query("SELECT * FROM bench_users").expect("warmup scan");
    }

    let loops = 500usize;
    let (min0, maj0) = read_faults();
    let t0 = Instant::now();
    let mut total_rows = 0usize;
    for _ in 0..loops {
        total_rows += db.query("SELECT * FROM bench_users").expect("scan").len();
    }
    let elapsed = t0.elapsed();
    let (min1, maj1) = read_faults();

    let per_scan_us = elapsed.as_secs_f64() * 1e6 / loops as f64;
    let per_row_ns = elapsed.as_secs_f64() * 1e9 / total_rows as f64;
    println!("── full_scan diagnose (WARM, {n_rows} rows, {loops} loops) ──");
    println!("  rows/scan:         {}", total_rows / loops);
    println!("  per-scan:          {per_scan_us:.1} µs");
    println!("  per-row:           {per_row_ns:.2} ns");
    println!(
        "  minor faults/scan: {:.3}",
        (min1.saturating_sub(min0)) as f64 / loops as f64
    );
    println!(
        "  major faults/scan: {:.3}",
        (maj1.saturating_sub(maj0)) as f64 / loops as f64
    );
    println!("  (≈0 faults/scan ⇒ warm scan served from buffer pool, NOT mmap)");
}

/// Isolate the per-lookup cost of the two AxiomDB point-lookup paths in steady
/// state: `db.query` (re-parse + run_cached plan cache) vs `PreparedStatement`
/// (clone analyzed AST + execute_with_ctx, no plan cache). Pinpoints why
/// "prepared" is slower than the cached query path for point lookups.
fn diagnose_point(data_dir: &Path, n_rows: usize) {
    let mut db = db_open(data_dir);
    reset(&mut db);
    let inserts = gen_inserts(n_rows);
    load_batch(&mut db, &inserts);
    let step = (n_rows / 100).max(1);
    let ids: Vec<usize> = (1..=n_rows).step_by(step).take(100).collect();

    for _ in 0..3 {
        for &i in &ids {
            let _ = db
                .query(&format!("SELECT * FROM bench_users WHERE id = {i}"))
                .expect("warm");
        }
    }

    let loops = 200usize;
    let total = loops * ids.len();

    let t0 = Instant::now();
    for _ in 0..loops {
        for &i in &ids {
            let _ = db
                .query(&format!("SELECT * FROM bench_users WHERE id = {i}"))
                .expect("q");
        }
    }
    let q_ns = t0.elapsed().as_secs_f64() * 1e9 / total as f64;

    let stmt = db
        .prepare("SELECT * FROM bench_users WHERE id = ?")
        .expect("prepare");
    #[cfg(feature = "bench-timings")]
    axiomdb_sql::bench_timings::reset_select_timings();
    let t0 = Instant::now();
    for _ in 0..loops {
        for &i in &ids {
            let _ = stmt
                .execute(&mut db, &[axiomdb_types::Value::Int(i as i32)])
                .expect("p");
        }
    }
    let p_ns = t0.elapsed().as_secs_f64() * 1e9 / total as f64;

    println!(
        "── point_lookup diagnose ({n_rows} rows, {loops}×{} lookups) ──",
        ids.len()
    );
    println!("  db.query  (parse + run_cached plan-cache):   {q_ns:.0} ns/lookup");
    println!("  prepared  (clone AST + execute):             {p_ns:.0} ns/lookup");

    // Phase breakdown of the prepared path (needs --features bench-timings).
    #[cfg(feature = "bench-timings")]
    {
        let s = axiomdb_sql::bench_timings::snapshot_select_timings();
        let n = s.calls.max(1) as f64;
        let clone = s.clone_ns as f64 / n;
        let resolve = s.resolve_ns as f64 / n;
        let stats = s.stats_ns as f64 / n;
        let plan = s.plan_ns as f64 / n;
        let lookup = s.lookup_ns as f64 / n;
        let where_eval = s.where_ns as f64 / n;
        let colmeta = s.colmeta_ns as f64 / n;
        let exec = s.exec_ns as f64 / n;
        let rest = exec - resolve - stats - plan - lookup - where_eval - colmeta;
        println!("    ├─ clone AST:              {clone:.0} ns");
        println!("    ├─ resolve_table:         {resolve:.0} ns  (Arc cache hit + key build)");
        println!("    ├─ list_stats (catalog):  {stats:.0} ns  (cached by schema_version)");
        println!("    ├─ planner:               {plan:.0} ns");
        println!(
            "    ├─ lookup (B-tree+decode):  {lookup:.0} ns  (irreducible ≈ SQLite seek+extract)"
        );
        println!(
            "    ├─ WHERE re-eval:           {where_eval:.0} ns  (skippable if key covers predicate)"
        );
        println!("    ├─ build_column_meta:     {colmeta:.0} ns  (ColumnMeta + name clones)");
        println!("    └─ project+result+rest:   {rest:.0} ns");
    }
}

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

    // The bench harness runs one scenario per `--scenario` invocation.
    // Each process exits at the end of `main`; with `SET synchronous =
    // NORMAL` (matching SQLite's bench config) the WAL may not be fully
    // flushed when the process terminates, leaving the .db in a state
    // recovery cannot reconcile cleanly on the next invocation
    // (ChecksumMismatch on `Db::open`). Each scenario already calls
    // `reset(&db)` to DROP+CREATE its table, so persisting state
    // between scenarios has no value — wipe the data dir before opening
    // to guarantee a clean baseline.
    for entry in std::fs::read_dir(&data_dir).into_iter().flatten().flatten() {
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
            if name.starts_with("bench.") || name.starts_with("diag_") {
                let _ = std::fs::remove_file(&path);
            }
        }
    }

    if args.contains(&"--diagnose-scan".to_string()) {
        diagnose_scan(&data_dir, n_rows);
    } else if args.contains(&"--diagnose-point".to_string()) {
        diagnose_point(&data_dir, n_rows);
    } else if args.contains(&"--diagnose".to_string()) {
        diagnose(&data_dir, n_rows);
    } else if args.contains(&"--diagnose-insert".to_string()) {
        diagnose_insert(&data_dir, n_rows);
    } else if args.contains(&"--diagnose-insert-deep".to_string()) {
        diagnose_insert_deep(&data_dir, n_rows);
    } else {
        run_scenario(&scenario, n_rows, &data_dir);
    }
}

// ── Deep INSERT diagnostic (requires --features axiomdb-sql/bench-timings) ────
#[cfg(feature = "bench-timings")]
fn diagnose_insert_deep(data_dir: &Path, n_rows: usize) {
    use axiomdb_catalog::bootstrap::CatalogBootstrap;
    use axiomdb_sql::{
        analyze_cached, bench_timings, bloom::BloomRegistry, execute_with_ctx, parse_with_sql_mode,
        SchemaCache, SessionContext,
    };
    use axiomdb_storage::MmapStorage;
    use axiomdb_wal::TxnManager;

    let _ = std::fs::remove_dir_all(data_dir);
    std::fs::create_dir_all(data_dir).unwrap();
    let db_path = data_dir.join("diag_insert_deep.db");
    let wal_path = data_dir.join("diag_insert_deep.wal");

    let storage = MmapStorage::create(&db_path).unwrap();
    CatalogBootstrap::init(&storage).unwrap();
    let txn = TxnManager::create(&wal_path).unwrap();
    let bloom = BloomRegistry::new();
    let mut schema_cache = SchemaCache::new();
    let mut session = SessionContext::default();
    let sql_mode = session.sql_mode_flags();

    let run = |sql: &str,
               storage: &MmapStorage,
               txn: &TxnManager,
               bloom: &BloomRegistry,
               cache: &mut SchemaCache,
               session: &mut SessionContext| {
        let stmt = parse_with_sql_mode(sql, None, sql_mode).unwrap();
        let analyzed = analyze_cached(stmt, storage, txn.snapshot(), cache).unwrap();
        execute_with_ctx(analyzed, storage, txn, bloom, session).unwrap();
    };

    run(
        "CREATE TABLE bench_users (id INT NOT NULL, name TEXT NOT NULL, \
         age INT NOT NULL, active BOOL NOT NULL, score REAL NOT NULL, \
         email TEXT NOT NULL, PRIMARY KEY (id))",
        &storage,
        &txn,
        &bloom,
        &mut schema_cache,
        &mut session,
    );
    run(
        "BEGIN",
        &storage,
        &txn,
        &bloom,
        &mut schema_cache,
        &mut session,
    );

    let sqls = gen_inserts(n_rows);

    // Warmup row (1 row) before resetting timers.
    let warm = parse_with_sql_mode(&sqls[0], None, sql_mode).unwrap();
    let warm = analyze_cached(warm, &storage, txn.snapshot(), &mut schema_cache).unwrap();
    execute_with_ctx(warm, &storage, &txn, &bloom, &mut session).unwrap();

    bench_timings::reset_insert_timings();

    for sql in &sqls[1..] {
        let stmt = parse_with_sql_mode(sql, None, sql_mode).unwrap();
        let snap = if let Some(ref ct) = session.conn_txn {
            txn.active_snapshot(ct)
        } else {
            txn.snapshot()
        };
        let analyzed = analyze_cached(stmt, &storage, snap, &mut schema_cache).unwrap();
        execute_with_ctx(analyzed, &storage, &txn, &bloom, &mut session).unwrap();
    }

    let t = bench_timings::snapshot_insert_timings();
    let rows = t.rows.max(1) as u128;
    let per = |ns: u128| ns / rows;

    let measured = [
        ("eval (expr → Value)", per(t.eval_ns)),
        ("validate exprs+defaults", per(t.validate_ns)),
        ("assign_auto_increment", per(t.auto_inc_ns)),
        ("materialize_generated", per(t.generated_cols_ns)),
        ("text+row constraints", per(t.constraints_ns)),
        ("fk_check_child_insert", per(t.fk_check_ns)),
        ("prepare_row (codec+PK)", per(t.prepare_row_ns)),
        ("validate_enum", per(t.enum_validate_ns)),
        ("pk_dup HashSet check", per(t.pk_dup_ns)),
        ("batch push (Vec+HashSet)", per(t.batch_push_ns)),
    ];
    let total: u128 = measured.iter().map(|(_, ns)| *ns).sum();

    fn fmt_us(ns: u128) -> String {
        format!("{:>8.2} µs", ns as f64 / 1000.0)
    }
    fn pct(part: u128, total: u128) -> String {
        if total == 0 {
            "  —".into()
        } else {
            format!("{:>6.1}%", part as f64 / total as f64 * 100.0)
        }
    }

    eprintln!();
    eprintln!("╔══════════════════════════════════════════════════════════════════╗");
    eprintln!(
        "║  AxiomDB INSERT Deep Phase Breakdown — {} rows (warmup discarded)",
        t.rows
    );
    eprintln!("║  Path: enqueue_clustered_insert_ctx per-row loop                 ║");
    eprintln!("╠══════════════════════════════════════════════════════════════════╣");
    eprintln!("║  Phase                              µs/row      % of measured    ║");
    eprintln!("║  ──────────────────────────────────────────────────────────      ║");
    for (name, ns) in &measured {
        eprintln!(
            "║  {:<32} {}    {}        ║",
            name,
            fmt_us(*ns),
            pct(*ns, total)
        );
    }
    eprintln!("║  ──────────────────────────────────────────────────────────      ║");
    eprintln!(
        "║  measured subtotal              {}                       ║",
        fmt_us(total)
    );
    eprintln!("║                                                                  ║");

    let mut sorted: Vec<_> = measured.to_vec();
    sorted.sort_by_key(|(_, ns)| std::cmp::Reverse(*ns));
    eprintln!("║  Top 3 phases:                                                   ║");
    for (i, (name, ns)) in sorted.iter().take(3).enumerate() {
        eprintln!(
            "║   {}. {:<28}  {}  ({})                  ║",
            i + 1,
            name,
            fmt_us(*ns),
            pct(*ns, total),
        );
    }
    eprintln!("╚══════════════════════════════════════════════════════════════════╝");
    eprintln!();
}

#[cfg(not(feature = "bench-timings"))]
fn diagnose_insert_deep(_data_dir: &Path, _n_rows: usize) {
    eprintln!();
    eprintln!("──────────────────────────────────────────────────────────────────");
    eprintln!("  --diagnose-insert-deep requires the bench-timings feature.");
    eprintln!("  Rebuild with:");
    eprintln!();
    eprintln!("      cargo run -p axiomdb-bench-comparison --release \\");
    eprintln!("          --features axiomdb-sql/bench-timings -- \\");
    eprintln!("          --scenario insert_batch --rows 10000 --diagnose-insert-deep");
    eprintln!("──────────────────────────────────────────────────────────────────");
    eprintln!();
}

// ── Diagnostic: timing breakdown per phase ────────────────────────────────────

fn diagnose(data_dir: &Path, n_rows: usize) {
    let mut db = db_open(data_dir);
    let inserts = gen_inserts(n_rows);
    load_batch(&mut db, &inserts);

    let iters = 200usize;
    let q_scan = "SELECT * FROM bench_users";
    let q_where = "SELECT * FROM bench_users WHERE active = TRUE";
    let q_count = "SELECT COUNT(*) FROM bench_users";
    let q_group = "SELECT age, COUNT(*) FROM bench_users GROUP BY age";

    // ── 1. Parse overhead (parse only, no catalog access) ─────────────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        axiomdb_sql::parse(q_scan, None).unwrap();
    }
    let parse_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── 2. Full run — full scan (parse + analyze_cached + execute) ────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        db_sql_count(&mut db, q_scan);
    }
    let scan_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── 3. Full run — scan with WHERE filter ──────────────────────────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        db_sql_count(&mut db, q_where);
    }
    let where_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── 4. Full run — COUNT(*) ────────────────────────────────────────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        db_sql(&mut db, q_count);
    }
    let count_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── 5. Full run — GROUP BY ────────────────────────────────────────────────
    let t0 = Instant::now();
    for _ in 0..iters {
        db_sql(&mut db, q_group);
    }
    let group_ns = t0.elapsed().as_nanos() as usize / iters;

    // ── Output ────────────────────────────────────────────────────────────────
    // With SchemaCache active, analyze_cached is near-zero after first call.
    // overhead = scan_ns - parse_ns ≈ analyze_cached (cache hit) + execute
    let overhead_ns = scan_ns.saturating_sub(parse_ns);

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
    eprintln!("║  Phase breakdown per query call (SchemaCache active):   ║");
    eprintln!("║                                                          ║");
    eprintln!(
        "║  parse()         {:>10}  ({} of scan total)      ║",
        fmt_us(parse_ns),
        pct(parse_ns, scan_ns)
    );
    eprintln!(
        "║  analyze+execute {:>10}  ({} of scan total)      ║",
        fmt_us(overhead_ns),
        pct(overhead_ns, scan_ns)
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
        n_rows as f64 / (scan_ns as f64 / 1e9)
    );
    eprintln!("╠══════════════════════════════════════════════════════════╣");
    eprintln!("║  Verdict:                                                ║");
    if parse_ns > overhead_ns / 2 {
        eprintln!("║  ⚠️  PARSE dominates — consider query plan cache        ║");
    } else {
        eprintln!("║  ✅ EXECUTE dominates — bottleneck is the heap scan     ║");
    }
    eprintln!("╚══════════════════════════════════════════════════════════╝");
    eprintln!();
}
