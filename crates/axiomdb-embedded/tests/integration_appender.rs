//! Integration tests for the embedded Appender fast-path INSERT API
//! (Attack 7, perf-sqlite-gap).
//!
//! Spec: `specs/fase-perf-sqlite-gap/spec-embedded-appender.md`
//! Plan: `specs/fase-perf-sqlite-gap/plan-embedded-appender.md`
//!
//! Step 1 here: the open path (`Db::appender(table)`) and its guards.
//! Subsequent steps add append/flush/finish/Drop and the edge-case
//! suite.

use axiomdb_core::error::DbError;
use axiomdb_embedded::Db;
use axiomdb_types::Value;
use tempfile::TempDir;

fn open_db() -> (TempDir, Db) {
    let dir = TempDir::new().unwrap();
    let db = Db::open(dir.path().join("test.db")).unwrap();
    (dir, db)
}

// ── Step 1 — Open path ────────────────────────────────────────────────────────

#[test]
fn appender_opens_on_heap_table() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let app = db.appender("t").unwrap();
    assert_eq!(app.pending(), 0, "freshly-opened appender has empty buffer");
}

#[test]
fn appender_open_on_missing_table_returns_table_not_found() {
    let (_dir, mut db) = open_db();
    let err = db.appender("ghost").unwrap_err();
    assert!(
        matches!(err, DbError::TableNotFound { .. }),
        "expected TableNotFound, got {err:?}"
    );
}

#[test]
fn appender_open_while_user_txn_open_returns_already_active() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    db.run("BEGIN").unwrap();
    let err = db.appender("t").unwrap_err();
    assert!(
        matches!(err, DbError::TransactionAlreadyActive { .. }),
        "expected TransactionAlreadyActive, got {err:?}"
    );
}

// ── Step 2 — append_row ───────────────────────────────────────────────────────

#[test]
fn append_row_accumulates_in_buffer() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("a".into())])
        .unwrap();
    app.append_row(&[Value::Int(2), Value::Text("b".into())])
        .unwrap();
    assert_eq!(app.pending(), 2);
}

#[test]
fn append_row_wrong_arity_returns_type_mismatch() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let err = app.append_row(&[Value::Int(1)]).unwrap_err();
    assert!(
        matches!(err, DbError::TypeMismatch { .. }),
        "expected TypeMismatch, got {err:?}"
    );
    // Appender remains usable after the rejected row.
    app.append_row(&[Value::Int(1), Value::Text("a".into())])
        .unwrap();
    assert_eq!(app.pending(), 1);
}

#[test]
fn append_row_not_null_violation_returns_error() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT NOT NULL, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let err = app
        .append_row(&[Value::Null, Value::Text("a".into())])
        .unwrap_err();
    assert!(
        matches!(err, DbError::NotNullViolation { .. }),
        "expected NotNullViolation, got {err:?}"
    );
    assert_eq!(app.pending(), 0, "rejected row not added to buffer");
}

#[test]
fn append_row_owned_consumes_vec() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let row = vec![Value::Int(1), Value::Text("a".into())];
    app.append_row_owned(row).unwrap();
    assert_eq!(app.pending(), 1);
}

#[test]
fn append_row_type_coercion_succeeds_in_permissive() {
    // strict_mode=ON (default) — coercion of a BigInt into Int succeeds
    // because the value fits (1 fits in i32). The point is to confirm
    // coerce_values_with_ctx is invoked.
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::BigInt(7)]).unwrap();
    assert_eq!(app.pending(), 1);
}

// ── Step 3 — flush (visibility tests deferred to Step 4 / finish) ─────────────

#[test]
fn flush_empty_buffer_is_noop() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.flush().unwrap();
    app.flush().unwrap();
    assert_eq!(app.pending(), 0);
}

#[test]
fn flush_drains_buffer_and_keeps_appender_usable() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("a".into())])
        .unwrap();
    app.append_row(&[Value::Int(2), Value::Text("b".into())])
        .unwrap();
    assert_eq!(app.pending(), 2);
    app.flush().unwrap();
    assert_eq!(app.pending(), 0, "buffer drained after flush");
    // Appender remains usable for more appends.
    app.append_row(&[Value::Int(3), Value::Text("c".into())])
        .unwrap();
    assert_eq!(app.pending(), 1);
}

// ── Step 4 (v1.1) — CHECK + text constraints ──────────────────────────────────

#[test]
fn appender_check_constraint_passes_when_satisfied() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, age INT CHECK (age >= 0))")
        .unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Int(25)]).unwrap();
    app.append_row(&[Value::Int(2), Value::Int(0)]).unwrap();
    app.finish().unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(2)
    );
}

#[test]
fn appender_check_constraint_violation_returns_error() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, age INT CHECK (age >= 0))")
        .unwrap();
    let mut app = db.appender("t").unwrap();
    let err = app.append_row(&[Value::Int(1), Value::Int(-5)]).unwrap_err();
    assert!(
        matches!(err, DbError::CheckViolation { .. }),
        "expected CheckViolation, got {err:?}"
    );
    // Appender remains usable for a valid row.
    app.append_row(&[Value::Int(2), Value::Int(30)]).unwrap();
    app.finish().unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(1)
    );
}

#[test]
fn appender_char_padding_applied() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, code CHAR(5))").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("ab".into())])
        .unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT code FROM t").unwrap();
    let stored = match &rows[0][0] {
        Value::Text(s) => s.clone(),
        v => panic!("expected Text, got {v:?}"),
    };
    // CHAR(5) right-pads with spaces.
    assert_eq!(stored.chars().count(), 5, "got {stored:?}");
}

// ── Step 2 (v1.1) — AUTO_INCREMENT support ────────────────────────────────────

#[test]
fn appender_assigns_auto_increment_on_null() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT AUTO_INCREMENT, v TEXT)")
        .unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Null, Value::Text("a".into())])
        .unwrap();
    app.append_row(&[Value::Null, Value::Text("b".into())])
        .unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT id, v FROM t ORDER BY id").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(1));
    assert_eq!(rows[1][0], Value::Int(2));
    assert_eq!(rows[0][1], Value::Text("a".into()));
}

#[test]
fn appender_respects_explicit_auto_increment_value() {
    // SQL semantics: explicit non-NULL wins, but AUTO_INC cache only
    // advances on Null-triggered assigns. So explicit 100 → keep,
    // next Null → next cache value (which seeded from existing scan).
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT AUTO_INCREMENT, v TEXT)")
        .unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(100), Value::Text("a".into())])
        .unwrap();
    app.finish().unwrap();
    // Reopen + insert via Null — cache scans existing MAX (100) and
    // continues from 101.
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Null, Value::Text("b".into())])
        .unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Value::Int(100));
    assert_eq!(rows[1][0], Value::Int(101));
}

// (Step 3's "flush on indexed table → NotImplemented" test was removed
// once Step 5 wired index maintenance. Positive index tests live below.)

// ── Step 4 — finish + Drop rollback ───────────────────────────────────────────

#[test]
fn finish_commits_and_returns_count() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 0..5 {
        app.append_row(&[Value::Int(i), Value::Text(format!("row{i}"))])
            .unwrap();
    }
    let n = app.finish().unwrap();
    assert_eq!(n, 5);
    // Rows are visible to subsequent queries on the same Db.
    let rows = db.query("SELECT id, v FROM t ORDER BY id").unwrap();
    assert_eq!(rows.len(), 5);
    assert_eq!(rows[0][0], Value::Int(0));
    assert_eq!(rows[4][1], Value::Text("row4".into()));
}

#[test]
fn finish_flushes_remaining_buffered_rows() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    // No explicit flush — finish() must flush before committing.
    app.append_row(&[Value::Int(42)]).unwrap();
    let n = app.finish().unwrap();
    assert_eq!(n, 1);
    let rows = db.query("SELECT id FROM t").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(42));
}

#[test]
fn finish_with_no_appends_commits_empty_txn() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let app = db.appender("t").unwrap();
    let n = app.finish().unwrap();
    assert_eq!(n, 0);
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(0)
    );
}

#[test]
fn drop_without_finish_rolls_back() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    {
        let mut app = db.appender("t").unwrap();
        app.append_row(&[Value::Int(1)]).unwrap();
        app.append_row(&[Value::Int(2)]).unwrap();
        // Drop without finish — txn rolled back; rows must NOT persist.
    }
    let count = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(count, Value::BigInt(0));
    // Subsequent appender works (no leaked txn state).
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(7)]).unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT id FROM t").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(7));
}

#[test]
fn drop_after_partial_flush_rolls_back_remaining_buffer() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    {
        let mut app = db.appender("t").unwrap();
        app.append_row(&[Value::Int(1)]).unwrap();
        app.flush().unwrap(); // row 1 written to heap, txn still open
        app.append_row(&[Value::Int(2)]).unwrap(); // buffered, not flushed
                                                   // Drop here — rollback should undo BOTH the flushed row and the
                                                   // buffered row (the whole txn aborts).
    }
    let count = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(
        count,
        Value::BigInt(0),
        "ALL rows including flushed ones roll back"
    );
}

// ── Step 5 — Secondary index maintenance ──────────────────────────────────────

#[test]
fn appender_maintains_secondary_btree_index() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    db.run("CREATE INDEX idx_v ON t (v)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("alpha".into())])
        .unwrap();
    app.append_row(&[Value::Int(2), Value::Text("beta".into())])
        .unwrap();
    app.append_row(&[Value::Int(3), Value::Text("gamma".into())])
        .unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT id FROM t WHERE v = 'beta'").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(2));
}

#[test]
fn appender_unique_index_violation_rolls_back_batch() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    db.run("CREATE UNIQUE INDEX idx_v ON t (v)").unwrap();
    db.run("INSERT INTO t VALUES (1, 'dup')").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(2), Value::Text("ok".into())])
        .unwrap();
    app.append_row(&[Value::Int(3), Value::Text("dup".into())])
        .unwrap();
    let err = app.finish().unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { .. }),
        "expected UniqueViolation, got {err:?}"
    );
    let n = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(n, Value::BigInt(1));
}

#[test]
fn appender_supports_table_with_multiple_indexes() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, age INT, name TEXT)")
        .unwrap();
    db.run("CREATE INDEX idx_age ON t (age)").unwrap();
    db.run("CREATE INDEX idx_name ON t (name)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 0..50 {
        app.append_row(&[
            Value::Int(i),
            Value::Int(20 + i % 30),
            Value::Text(format!("user_{i}")),
        ])
        .unwrap();
    }
    app.finish().unwrap();
    let by_age = db.query("SELECT COUNT(*) FROM t WHERE age = 25").unwrap();
    assert!(matches!(by_age[0][0], Value::BigInt(n) if n > 0));
    let by_name = db.query("SELECT id FROM t WHERE name = 'user_42'").unwrap();
    assert_eq!(by_name.len(), 1);
    assert_eq!(by_name[0][0], Value::Int(42));
}

// ── Step 6 — auto-flush at 1024 rows ──────────────────────────────────────────

#[test]
fn auto_flush_keeps_buffer_bounded() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let mut max_pending = 0usize;
    for i in 0..5000 {
        app.append_row(&[Value::Int(i)]).unwrap();
        max_pending = max_pending.max(app.pending());
    }
    // Threshold is 1024; the buffer hits 1024, auto-flushes, drops to 0.
    // After 5000 appends max_pending should be exactly 1024.
    assert!(
        max_pending <= 1024,
        "buffer exceeded threshold: max_pending = {max_pending}"
    );
    let n = app.finish().unwrap();
    assert_eq!(n, 5000);
}

#[test]
fn appender_loads_50k_rows() {
    // 50k is enough to exercise multiple auto-flushes (~49 of them) without
    // making the test slow on Lima. The 100k case from the plan is covered
    // implicitly by the same code path.
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 0..50_000i64 {
        app.append_row(&[Value::Int(i as i32), Value::Int((i * 2) as i32)])
            .unwrap();
    }
    let n = app.finish().unwrap();
    assert_eq!(n, 50_000);
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(50_000)
    );
}

// ── Step 8 — Remaining edge cases from the spec ───────────────────────────────

// ── Step 5 (v1.1) — FOREIGN KEY constraints ───────────────────────────────────

#[test]
fn appender_fk_to_existing_parent_succeeds() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.run("INSERT INTO parent VALUES (1), (2)").unwrap();
    db.run("CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id))")
        .unwrap();
    let mut app = db.appender("child").unwrap();
    app.append_row(&[Value::Int(10), Value::Int(1)]).unwrap();
    app.append_row(&[Value::Int(11), Value::Int(2)]).unwrap();
    app.finish().unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) FROM child").unwrap()[0][0],
        Value::BigInt(2)
    );
}

#[test]
fn appender_fk_to_missing_parent_returns_error() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.run("INSERT INTO parent VALUES (1)").unwrap();
    db.run("CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id))")
        .unwrap();
    let mut app = db.appender("child").unwrap();
    let err = app
        .append_row(&[Value::Int(10), Value::Int(999)])
        .unwrap_err();
    assert!(
        matches!(err, DbError::ForeignKeyViolation { .. }),
        "expected ForeignKeyViolation, got {err:?}"
    );
}

#[test]
fn appender_fk_null_child_is_match_simple_pass() {
    // SQL standard MATCH SIMPLE: any NULL in the FK columns passes.
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE parent (id INT PRIMARY KEY)").unwrap();
    db.run("CREATE TABLE child (id INT, parent_id INT REFERENCES parent(id))")
        .unwrap();
    let mut app = db.appender("child").unwrap();
    app.append_row(&[Value::Int(10), Value::Null]).unwrap();
    app.finish().unwrap();
    assert_eq!(
        db.query("SELECT COUNT(*) FROM child").unwrap()[0][0],
        Value::BigInt(1)
    );
}

// ── Step 3 (v1.1) — GENERATED columns support ─────────────────────────────────

#[test]
fn appender_materializes_stored_generated_column_on_null() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v INT, doubled INT GENERATED ALWAYS AS (v * 2) STORED)")
        .unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Int(5), Value::Null])
        .unwrap();
    app.append_row(&[Value::Int(2), Value::Int(7), Value::Null])
        .unwrap();
    app.finish().unwrap();
    let rows = db.query("SELECT v, doubled FROM t ORDER BY v").unwrap();
    assert_eq!(rows[0][1], Value::Int(10));
    assert_eq!(rows[1][1], Value::Int(14));
}

#[test]
fn appender_rejects_explicit_generated_always_value() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v INT, doubled INT GENERATED ALWAYS AS (v * 2) STORED)")
        .unwrap();
    let mut app = db.appender("t").unwrap();
    let err = app
        .append_row(&[Value::Int(1), Value::Int(5), Value::Int(999)])
        .unwrap_err();
    // Whatever the helper raises (Other / TypeMismatch / InvalidValue);
    // semantic is "non-NULL value for GENERATED ALWAYS rejected".
    assert!(
        !matches!(err, DbError::NotNullViolation { .. }),
        "should not be NOT NULL — got {err:?}"
    );
}

#[test]
fn appender_normalizes_text_nfc() {
    // Both 'café' forms (NFC 5 bytes + NFD 6 bytes) must store identically.
    // The Appender goes through encode_row which applies NFC.
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    let nfc = "café".to_string();
    // 'cafe' + combining acute (U+0301) = NFD form, 6 bytes
    let nfd = "cafe\u{0301}".to_string();
    assert_ne!(nfc.as_bytes().len(), nfd.as_bytes().len()); // sanity
    app.append_row(&[Value::Int(1), Value::Text(nfc)]).unwrap();
    app.append_row(&[Value::Int(2), Value::Text(nfd)]).unwrap();
    app.finish().unwrap();
    // Both rows must compare equal under '='.
    let rows = db.query("SELECT COUNT(*) FROM t WHERE v = 'café'").unwrap();
    assert_eq!(
        rows[0][0],
        Value::BigInt(2),
        "both NFC and NFD inputs must normalize to the same stored form"
    );
}

#[test]
fn appender_writes_visible_only_after_finish() {
    // The Appender holds &mut Db for its lifetime — the borrow checker
    // enforces that no concurrent operation can run on the SAME Db
    // handle while the appender is alive (compile-time guarantee). So
    // this test verifies the post-finish visibility transition: rows
    // appended into a dropped Appender are NOT visible, while rows
    // committed via finish() ARE.
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    db.run("INSERT INTO t VALUES (1)").unwrap();
    {
        let mut app = db.appender("t").unwrap();
        app.append_row(&[Value::Int(2)]).unwrap();
        app.append_row(&[Value::Int(3)]).unwrap();
        // Drop without finish — buffered writes discarded.
    }
    let n = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(n, Value::BigInt(1), "dropped appender did not persist");
    // Now the finish() path:
    {
        let mut app = db.appender("t").unwrap();
        app.append_row(&[Value::Int(4)]).unwrap();
        app.finish().unwrap();
    }
    let n = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(n, Value::BigInt(2), "finished appender writes persist");
}

#[test]
fn appender_honors_set_synchronous_normal() {
    // Functional smoke: after SET synchronous = NORMAL the appender's
    // commit uses flush_no_sync. Data must still persist within the
    // process lifetime (we can query it back).
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    db.run("SET synchronous = 'NORMAL'").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 0..50 {
        app.append_row(&[Value::Int(i)]).unwrap();
    }
    let n = app.finish().unwrap();
    assert_eq!(n, 50);
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(50)
    );
}

#[test]
fn appender_pending_returns_zero_initially() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT)").unwrap();
    let app = db.appender("t").unwrap();
    assert_eq!(app.pending(), 0);
}

// ── Step 6 (v1.1) — Clustered tables ──────────────────────────────────────────

#[test]
fn appender_works_on_clustered_table() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 1..=10 {
        app.append_row(&[Value::Int(i), Value::Text(format!("row{i}"))])
            .unwrap();
    }
    let n = app.finish().unwrap();
    assert_eq!(n, 10);
    let rows = db.query("SELECT id FROM t ORDER BY id").unwrap();
    assert_eq!(rows.len(), 10);
    for (i, row) in rows.iter().enumerate() {
        assert_eq!(row[0], Value::Int((i + 1) as i32));
    }
}

#[test]
fn appender_clustered_pk_duplicate_returns_error() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.run("INSERT INTO t VALUES (1, 'a')").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(2), Value::Text("b".into())])
        .unwrap();
    app.append_row(&[Value::Int(1), Value::Text("dup".into())])
        .unwrap();
    let err = app.finish().unwrap_err();
    assert!(
        matches!(err, DbError::UniqueViolation { .. } | DbError::DuplicateKey { .. }),
        "expected UniqueViolation or DuplicateKey, got {err:?}"
    );
    // Pre-existing row 1 still there; appender rows rolled back.
    let n = db.query("SELECT COUNT(*) FROM t").unwrap()[0][0].clone();
    assert_eq!(n, Value::BigInt(1));
}

#[test]
fn appender_clustered_with_secondary_index() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT PRIMARY KEY, v TEXT)").unwrap();
    db.run("CREATE INDEX idx_v ON t (v)").unwrap();
    let mut app = db.appender("t").unwrap();
    app.append_row(&[Value::Int(1), Value::Text("alpha".into())])
        .unwrap();
    app.append_row(&[Value::Int(2), Value::Text("beta".into())])
        .unwrap();
    app.append_row(&[Value::Int(3), Value::Text("gamma".into())])
        .unwrap();
    app.finish().unwrap();
    // Lookup via the secondary index path.
    let rows = db.query("SELECT id FROM t WHERE v = 'beta'").unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Value::Int(2));
}

#[test]
fn appender_clustered_50k_rows() {
    let (_dir, mut db) = open_db();
    db.run("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
    let mut app = db.appender("t").unwrap();
    for i in 0..50_000i64 {
        app.append_row(&[Value::Int(i as i32), Value::Int((i * 2) as i32)])
            .unwrap();
    }
    let n = app.finish().unwrap();
    assert_eq!(n, 50_000);
    assert_eq!(
        db.query("SELECT COUNT(*) FROM t").unwrap()[0][0],
        Value::BigInt(50_000)
    );
}
