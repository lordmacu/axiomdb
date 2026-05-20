/// Correctness verification for all Appender-based benchmark scenarios.
///
/// Each test replicates the workload of its corresponding benchmark scenario
/// and then verifies that data inserted via the Appender matches the
/// deterministic formula exactly — counts, aggregates, point lookups, and
/// index lookups.
///
/// Data formula (same as gen_inserts in axiomdb_bench, 1-based):
///   id     = i
///   name   = format!("user_{i:06}")
///   age    = 18 + (i % 62)                   → [18, 79]
///   active = i % 2 == 0                       → TRUE for even ids
///   score  = 100.0 + (i % 1000) as f64 * 0.1 → [100.0, 199.9]
///   email  = format!("u{i}@b.local")
use axiomdb_embedded::Db;
use axiomdb_types::Value;
use tempfile::TempDir;

// ── helpers ───────────────────────────────────────────────────────────────────

fn open_db() -> (TempDir, Db) {
    let dir = TempDir::new().expect("tempdir");
    let db = Db::open(dir.path().join("test.db")).expect("Db::open");
    (dir, db)
}

fn sql(db: &mut Db, q: &str) {
    db.run(q)
        .unwrap_or_else(|e| panic!("SQL failed: {q}\nError: {e:?}"));
}

fn qrows(db: &mut Db, q: &str) -> Vec<Vec<Value>> {
    db.query(q)
        .unwrap_or_else(|e| panic!("query failed: {q}\nError: {e:?}"))
}

fn scalar_int(db: &mut Db, q: &str) -> i64 {
    match qrows(db, q)
        .into_iter()
        .next()
        .and_then(|r| r.into_iter().next())
    {
        Some(Value::Int(v)) => v as i64,
        Some(Value::BigInt(v)) => v,
        other => panic!("expected integer scalar from '{q}', got {other:?}"),
    }
}

fn create_clustered_table(db: &mut Db) {
    sql(db, "DROP TABLE IF EXISTS bench_users");
    sql(
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

fn create_heap_table(db: &mut Db) {
    sql(db, "DROP TABLE IF EXISTS bench_users_heap");
    sql(
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
}

fn check_row(row: &[Value], id: usize) {
    assert_eq!(row[0], Value::Int(id as i32), "id mismatch at id={id}");
    assert_eq!(
        row[1],
        Value::Text(format!("user_{id:06}")),
        "name mismatch at id={id}"
    );
    assert_eq!(
        row[2],
        Value::Int((18 + id % 62) as i32),
        "age mismatch at id={id}"
    );
    assert_eq!(
        row[3],
        Value::Bool(id % 2 == 0),
        "active mismatch at id={id}"
    );
    let expected_score = 100.0 + (id % 1000) as f64 * 0.1;
    match row[4] {
        Value::Real(got) => assert!(
            (got - expected_score).abs() < 0.001,
            "score mismatch at id={id}: expected {expected_score:.1}, got {got}",
        ),
        ref other => panic!("expected Real for score at id={id}, got {other:?}"),
    }
    assert_eq!(
        row[5],
        Value::Text(format!("u{id}@b.local")),
        "email mismatch at id={id}"
    );
}

/// Append N rows to `table` using `append_row_owned`.
fn append_rows(db: &mut Db, table: &str, n: usize) {
    let mut app = db.appender(table).expect("open appender");
    for i in 1..=n {
        app.append_row_owned(vec![
            Value::Int(i as i32),
            Value::Text(format!("user_{i:06}")),
            Value::Int((18 + (i % 62)) as i32),
            Value::Bool(i % 2 == 0),
            Value::Real(100.0 + (i % 1000) as f64 * 0.1),
            Value::Text(format!("u{i}@b.local")),
        ])
        .unwrap_or_else(|e| panic!("append_row_owned failed at i={i}: {e:?}"));
    }
    let committed = app.finish().expect("appender finish");
    assert_eq!(committed, n as u64, "finish must report {n} committed rows");
}

// ── 1. insert_appender (clustered) ───────────────────────────────────────────

/// insert_appender scenario: Appender on a clustered table (PRIMARY KEY id).
///
/// Detects: rows dropped by the clustered B-tree flush, PK constraint
/// silently violated, column ordering wrong in the serialized row.
#[test]
fn appender_clustered_count_aggregates_and_spot_checks_correct() {
    const N: usize = 300;
    let (_dir, mut db) = open_db();
    create_clustered_table(&mut db);
    append_rows(&mut db, "bench_users", N);

    // COUNT(*) must be exactly N.
    let count = scalar_int(&mut db, "SELECT COUNT(*) FROM bench_users");
    assert_eq!(count, N as i64, "COUNT(*) must be {N} after appender flush");

    // SUM(id) = N*(N+1)/2 — catches phantom or missing rows.
    let sum_id = scalar_int(&mut db, "SELECT SUM(id) FROM bench_users");
    assert_eq!(sum_id, (N as i64) * (N as i64 + 1) / 2, "SUM(id) mismatch");

    // MIN/MAX age from formula.
    let min_age = scalar_int(&mut db, "SELECT MIN(age) FROM bench_users");
    let max_age = scalar_int(&mut db, "SELECT MAX(age) FROM bench_users");
    assert_eq!(min_age, 18, "MIN(age) must be 18");
    assert_eq!(max_age, 79, "MAX(age) must be 79");

    // active=TRUE count = N/2.
    let active = scalar_int(
        &mut db,
        "SELECT COUNT(*) FROM bench_users WHERE active = TRUE",
    );
    assert_eq!(active, (N / 2) as i64, "active count must be N/2");

    // MIN/MAX score from formula (computed for the exact N used).
    let expected_min_score = 100.0 + (1..=N).map(|i| i % 1000).min().unwrap() as f64 * 0.1;
    let expected_max_score = 100.0 + (1..=N).map(|i| i % 1000).max().unwrap() as f64 * 0.1;
    let min_score_rows = qrows(&mut db, "SELECT MIN(score) FROM bench_users");
    if let Value::Real(v) = min_score_rows[0][0] {
        assert!(
            (v - expected_min_score).abs() < 0.001,
            "MIN(score) must be ~{expected_min_score:.1}, got {v}"
        );
    } else {
        panic!("MIN(score) not Real: {:?}", min_score_rows[0][0]);
    }
    let max_score_rows = qrows(&mut db, "SELECT MAX(score) FROM bench_users");
    if let Value::Real(v) = max_score_rows[0][0] {
        assert!(
            (v - expected_max_score).abs() < 0.001,
            "MAX(score) must be ~{expected_max_score:.1}, got {v}"
        );
    }

    // Spot-check specific rows via PK lookup.
    for &id in &[1usize, 50, 100, 200, 299, 300] {
        let result = qrows(
            &mut db,
            &format!("SELECT id, name, age, active, score, email FROM bench_users WHERE id = {id}"),
        );
        assert_eq!(result.len(), 1, "PK lookup id={id} must return 1 row");
        check_row(&result[0], id);
    }
}

// ── 2. insert_appender_heap ───────────────────────────────────────────────────

/// insert_appender_heap scenario: Appender on a heap-only table (no PK).
///
/// Detects: appender dropping rows without B-tree ordering, COUNT wrong,
/// heap pages written in wrong order causing value corruption.
#[test]
fn appender_heap_count_aggregates_and_spot_checks_correct() {
    const N: usize = 300;
    let (_dir, mut db) = open_db();
    create_heap_table(&mut db);
    append_rows(&mut db, "bench_users_heap", N);

    let count = scalar_int(&mut db, "SELECT COUNT(*) FROM bench_users_heap");
    assert_eq!(count, N as i64, "COUNT(*) must be {N} on heap table");

    let sum_id = scalar_int(&mut db, "SELECT SUM(id) FROM bench_users_heap");
    assert_eq!(
        sum_id,
        (N as i64) * (N as i64 + 1) / 2,
        "SUM(id) mismatch on heap"
    );

    let min_age = scalar_int(&mut db, "SELECT MIN(age) FROM bench_users_heap");
    let max_age = scalar_int(&mut db, "SELECT MAX(age) FROM bench_users_heap");
    assert_eq!(min_age, 18);
    assert_eq!(max_age, 79);

    let active = scalar_int(
        &mut db,
        "SELECT COUNT(*) FROM bench_users_heap WHERE active = TRUE",
    );
    assert_eq!(active, (N / 2) as i64);

    // Heap has no PK index — point lookup goes through heap scan with filter.
    for &id in &[1usize, 50, 150, 250, 300] {
        let result = qrows(
            &mut db,
            &format!(
                "SELECT id, name, age, active, score, email FROM bench_users_heap WHERE id = {id}"
            ),
        );
        assert_eq!(result.len(), 1, "heap scan for id={id} must return 1 row");
        check_row(&result[0], id);
    }
}

// ── 3. insert_appender_large (multi-flush) ────────────────────────────────────

/// insert_appender_large scenario: N=5000 rows, which triggers multiple
/// automatic flushes (APPENDER_BATCH_FLUSH = 1024).
///
/// Detects: rows dropped or duplicated across flush boundaries, first or last
/// chunk missing, B-tree root split not handled between flushes.
#[test]
fn appender_large_multi_flush_all_rows_correct() {
    // 5000 rows → ~5 auto-flushes at 1024 threshold.
    const N: usize = 5_000;
    let (_dir, mut db) = open_db();
    create_clustered_table(&mut db);
    append_rows(&mut db, "bench_users", N);

    // COUNT must be exactly N — any dropped or doubled row fails this.
    let count = scalar_int(&mut db, "SELECT COUNT(*) FROM bench_users");
    assert_eq!(count, N as i64, "all {N} rows must survive multi-flush");

    // SUM(id) = N*(N+1)/2 — uniquely identifies the exact multiset of ids.
    let expected_sum = (N as i64) * (N as i64 + 1) / 2;
    let sum_id = scalar_int(&mut db, "SELECT SUM(id) FROM bench_users");
    assert_eq!(
        sum_id, expected_sum,
        "SUM(id) must match Gauss formula after multi-flush"
    );

    // MIN score=100.0 at id=1000 (and 2000, 3000, etc.), MAX=199.9 at id=999.
    let min_score_rows = qrows(&mut db, "SELECT MIN(score) FROM bench_users");
    if let Value::Real(v) = min_score_rows[0][0] {
        assert!(
            (v - 100.0).abs() < 0.001,
            "MIN(score) must be ~100.0 after multi-flush"
        );
    }
    let max_score_rows = qrows(&mut db, "SELECT MAX(score) FROM bench_users");
    if let Value::Real(v) = max_score_rows[0][0] {
        assert!(
            (v - 199.9).abs() < 0.001,
            "MAX(score) must be ~199.9 after multi-flush"
        );
    }

    // Spot-check rows at flush-boundary positions (multiples of 1024 ± 1).
    for &id in &[
        1, 1023, 1024, 1025, 2047, 2048, 2049, 3071, 3072, 3073, 4095, 4096, 4097, 5000,
    ] {
        let result = qrows(
            &mut db,
            &format!("SELECT id, name, age, active, score, email FROM bench_users WHERE id = {id}"),
        );
        assert_eq!(
            result.len(),
            1,
            "id={id} (flush boundary) must exist after multi-flush"
        );
        check_row(&result[0], id);
    }
}

// ── 4. insert_appender_indexed ────────────────────────────────────────────────

/// insert_appender_indexed scenario: Appender on clustered table WITH a
/// secondary index (idx_email).  Index entries must be present and correct.
///
/// Detects: secondary index entries missing after appender flush, wrong
/// page pointer in index leaf, COUNT(*) changed by index creation.
#[test]
fn appender_indexed_secondary_index_lookup_correct() {
    const N: usize = 300;
    let (_dir, mut db) = open_db();
    create_clustered_table(&mut db);
    sql(&mut db, "CREATE INDEX idx_email ON bench_users (email)");
    append_rows(&mut db, "bench_users", N);

    // COUNT unchanged by the secondary index.
    let count = scalar_int(&mut db, "SELECT COUNT(*) FROM bench_users");
    assert_eq!(count, N as i64, "COUNT must be {N} with secondary index");

    // Query specific rows via the email secondary index.
    for &id in &[1usize, 42, 100, 200, 299, 300] {
        let email = format!("u{id}@b.local");
        let result = qrows(
            &mut db,
            &format!(
                "SELECT id, name, age, active, score, email \
             FROM bench_users WHERE email = '{email}'"
            ),
        );
        assert_eq!(
            result.len(),
            1,
            "index lookup email='{email}' must return exactly 1 row"
        );
        check_row(&result[0], id);
    }

    // Non-existent email returns 0 rows.
    let ghost = qrows(
        &mut db,
        "SELECT id FROM bench_users WHERE email = 'ghost@b.local'",
    );
    assert_eq!(
        ghost.len(),
        0,
        "index lookup for missing email must return 0 rows"
    );

    // Aggregates must still be correct.
    let sum_id = scalar_int(&mut db, "SELECT SUM(id) FROM bench_users");
    assert_eq!(
        sum_id,
        (N as i64) * (N as i64 + 1) / 2,
        "SUM(id) mismatch with secondary index"
    );
}

// ── 5. insert_appender_typed (typed builder API) ──────────────────────────────

/// insert_appender_typed scenario: typed builder (append_int / append_text /
/// append_bool / append_real / end_row) on a heap table.
///
/// Detects: wrong column mapping in the typed builder, end_row forgetting to
/// seal the row, type coercion applying wrong transformation.
#[test]
fn appender_typed_builder_inserts_correct_values() {
    const N: usize = 100;
    let (_dir, mut db) = open_db();
    sql(&mut db, "DROP TABLE IF EXISTS bench_users_heap");
    sql(
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

    let mut app = db.appender("bench_users_heap").expect("open appender");
    for i in 1..=N {
        app.append_int(i as i32).unwrap();
        app.append_text(&format!("user_{i:06}")).unwrap();
        app.append_int((18 + (i % 62)) as i32).unwrap();
        app.append_bool(i % 2 == 0).unwrap();
        app.append_real(100.0 + (i % 1000) as f64 * 0.1).unwrap();
        app.append_text(&format!("u{i}@b.local")).unwrap();
        app.end_row().unwrap();
    }
    let committed = app.finish().expect("appender finish");
    assert_eq!(
        committed, N as u64,
        "typed builder finish must report {N} committed rows"
    );

    // Verify count.
    let count = scalar_int(&mut db, "SELECT COUNT(*) FROM bench_users_heap");
    assert_eq!(count, N as i64, "COUNT(*) must be {N} after typed builder");

    // SUM and aggregates.
    let sum_id = scalar_int(&mut db, "SELECT SUM(id) FROM bench_users_heap");
    assert_eq!(sum_id, (N as i64) * (N as i64 + 1) / 2, "SUM(id) mismatch");

    let min_age = scalar_int(&mut db, "SELECT MIN(age) FROM bench_users_heap");
    let max_age = scalar_int(&mut db, "SELECT MAX(age) FROM bench_users_heap");
    assert_eq!(min_age, 18);
    assert_eq!(max_age, 79);

    // Spot-check specific rows — verifies column order and type correctness.
    for &id in &[1usize, 25, 50, 62, 75, 100] {
        let result = qrows(
            &mut db,
            &format!(
                "SELECT id, name, age, active, score, email FROM bench_users_heap WHERE id = {id}"
            ),
        );
        assert_eq!(result.len(), 1, "id={id} must exist after typed builder");
        check_row(&result[0], id);
    }
}

// ── 6. appender + create_index after load ─────────────────────────────────────

/// create_index scenario: load N rows via appender, then CREATE INDEX,
/// verify that the index is consistent with the heap after bulk-index build.
///
/// Detects: bulk-index build reading wrong rows, index built before appender
/// flushes (race), missing entries at page boundaries.
#[test]
fn create_index_after_appender_load_is_consistent() {
    const N: usize = 1000;
    let (_dir, mut db) = open_db();
    create_clustered_table(&mut db);
    append_rows(&mut db, "bench_users", N);

    // Baseline aggregates before index.
    let count_before = scalar_int(&mut db, "SELECT COUNT(*) FROM bench_users");
    let sum_before = scalar_int(&mut db, "SELECT SUM(id) FROM bench_users");

    // Create index.
    sql(&mut db, "CREATE INDEX idx_email ON bench_users (email)");

    // Aggregates must be unchanged.
    let count_after = scalar_int(&mut db, "SELECT COUNT(*) FROM bench_users");
    let sum_after = scalar_int(&mut db, "SELECT SUM(id) FROM bench_users");
    assert_eq!(
        count_after, count_before,
        "COUNT(*) must not change after CREATE INDEX"
    );
    assert_eq!(
        sum_after, sum_before,
        "SUM(id) must not change after CREATE INDEX"
    );

    // Index lookups at various positions.
    for &id in &[1usize, 100, 500, 1000] {
        let email = format!("u{id}@b.local");
        let result = qrows(
            &mut db,
            &format!(
                "SELECT id, name, age, active, score, email \
             FROM bench_users WHERE email = '{email}'"
            ),
        );
        assert_eq!(
            result.len(),
            1,
            "post-index lookup email='{email}' must return 1 row"
        );
        check_row(&result[0], id);
    }
}

// ── 7. crud_flow via appender ─────────────────────────────────────────────────

/// crud_flow scenario via Appender: insert via appender, full scan, delete all.
///
/// Validates that the full lifecycle (Appender insert → SQL SELECT → SQL DELETE)
/// works correctly end-to-end.
#[test]
fn crud_flow_via_appender_insert_scan_delete_correct() {
    const N: usize = 500;
    let (_dir, mut db) = open_db();
    create_clustered_table(&mut db);
    append_rows(&mut db, "bench_users", N);

    // Full scan after appender insert.
    let count = scalar_int(&mut db, "SELECT COUNT(*) FROM bench_users");
    assert_eq!(count, N as i64, "count after appender insert must be {N}");

    let all = qrows(
        &mut db,
        "SELECT id, name, age, active, score, email FROM bench_users",
    );
    assert_eq!(all.len(), N, "full scan must return {N} rows");

    // Spot-check a few rows from the full scan result.
    for &id in &[1usize, 100, 250, 499, 500] {
        let row = all
            .iter()
            .find(|r| r[0] == Value::Int(id as i32))
            .unwrap_or_else(|| panic!("id={id} missing from full scan"));
        check_row(row, id);
    }

    // Delete all rows.
    sql(&mut db, "DELETE FROM bench_users");

    let after = scalar_int(&mut db, "SELECT COUNT(*) FROM bench_users");
    assert_eq!(after, 0, "DELETE all must leave 0 rows, got {after}");

    // Scan after delete must return empty (not panic).
    let empty = qrows(&mut db, "SELECT id FROM bench_users");
    assert_eq!(empty.len(), 0, "scan after DELETE all must return 0 rows");
}
