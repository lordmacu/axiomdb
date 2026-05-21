//! Regression tests for bulk index builds over an *already-populated* clustered
//! table, under the server's deferred (group-commit) durability mode.
//!
//! Bug (fixed): `CREATE INDEX … USING GIN (col)` — and any clustered bulk index
//! build — on a clustered table that already held data built an empty index, so
//! `WHERE col @> '{…}'` returned 0. Two root causes, both reproduced here:
//!
//!   * Layer 1 — the build scanned the *stale catalog* `root_page_id`. After the
//!     inserts split the clustered B-tree, the live root lives in memory and the
//!     catalog lags, so the scan started from the wrong page.
//!   * Layer 2 — `CREATE INDEX` is DDL, which implicitly commits the open
//!     transaction and runs the build in a *fresh* transaction. In deferred
//!     commit mode the implicit commit removed the txn from the active set but
//!     did NOT advance `max_committed`, so the new transaction's snapshot could
//!     not see the just-committed rows and the scan returned 0 rows.
//!
//! The harness mirrors the server: `deferred_commit_mode = true` and a driver
//! that advances `max_committed` only when the (post-statement) deferred commit
//! is driven — exactly the window in which the bug manifested. Enough padded
//! rows are inserted to force clustered page splits so Layer 1 is exercised too.

mod common;

use axiomdb_catalog::{CatalogBootstrap, CatalogReader, IndexDef};
use axiomdb_core::error::DbError;
use axiomdb_sql::clustered_secondary::ClusteredSecondaryLayout;
use axiomdb_sql::{
    analyze_with_defaults, bloom::BloomRegistry, execute_with_ctx, parse, QueryResult,
    SessionContext,
};
use axiomdb_storage::MemoryStorage;
use axiomdb_types::Value;
use axiomdb_wal::TxnManager;

// ── harness: deferred commit mode (mirrors the server) ─────────────────────────

/// Like `common::setup_ctx` but enables the server's deferred (group-commit)
/// durability mode, under which `commit()` does not advance `max_committed`
/// until the pipeline is driven.
fn setup_deferred() -> (MemoryStorage, TxnManager, BloomRegistry, SessionContext) {
    let dir = tempfile::tempdir().unwrap();
    let wal_path = dir.keep().join("test.wal");
    let storage = MemoryStorage::new();
    CatalogBootstrap::init(&storage).unwrap();
    let mut txn = TxnManager::create(&wal_path).unwrap();
    txn.set_deferred_commit_mode(true);
    let bloom = BloomRegistry::new();
    let ctx = SessionContext::new();
    (storage, txn, bloom, ctx)
}

/// Runs one statement and then drives any deferred commit to completion
/// (fsync + advance `max_committed`), mirroring the network layer's
/// `take_commit_rx`. This makes committed rows visible to *later* snapshots —
/// but not within the same statement, which is the condition the bug needed.
fn run_drv(
    sql: &str,
    storage: &mut MemoryStorage,
    txn: &mut TxnManager,
    bloom: &mut BloomRegistry,
    ctx: &mut SessionContext,
) -> Result<QueryResult, DbError> {
    let stmt = parse(sql, None)?;
    let snap = if let Some(ref ct) = ctx.conn_txn {
        txn.active_snapshot(ct)
    } else {
        txn.snapshot()
    };
    let analyzed = analyze_with_defaults(
        stmt,
        storage,
        snap,
        ctx.effective_database(),
        ctx.current_schema(),
    )?;
    let result = execute_with_ctx(analyzed, storage, txn, bloom, ctx);
    if let Some(pending) = ctx.pending_deferred_txn_id.take() {
        txn.wal_flush_and_fsync()?;
        txn.advance_committed_single(pending);
    }
    result
}

fn scalar_count(result: QueryResult) -> i64 {
    match result {
        QueryResult::Rows { rows, .. } => match rows.first().and_then(|r| r.first()) {
            Some(Value::BigInt(n)) => *n,
            Some(Value::Int(n)) => *n as i64,
            other => panic!("expected a count scalar, got {other:?}"),
        },
        other => panic!("expected Rows, got {other:?}"),
    }
}

fn table_indexes(storage: &MemoryStorage, txn: &TxnManager, table: &str) -> Vec<IndexDef> {
    let snap = txn.snapshot();
    let mut reader = CatalogReader::new(storage, snap).unwrap();
    let def = reader.get_table("public", table).unwrap().unwrap();
    reader.list_indexes(def.id).unwrap()
}

/// Number of physical entries currently stored in a clustered secondary index —
/// proves the bulk build actually indexed the existing rows.
fn secondary_entry_count(storage: &MemoryStorage, indexes: &[IndexDef], name: &str) -> usize {
    let primary = indexes.iter().find(|i| i.is_primary).unwrap();
    let secondary = indexes.iter().find(|i| i.name == name).unwrap();
    let layout = ClusteredSecondaryLayout::derive(secondary, primary).unwrap();
    layout
        .scan_prefix(storage, secondary.root_page_id, &[])
        .unwrap()
        .len()
}

// Enough padded rows to force several clustered leaf splits (16 KiB pages) so the
// catalog root_page_id goes stale relative to the live in-memory root.
const N: i64 = 300;
const PAD: usize = 200;

fn active_for(i: i64) -> i64 {
    i % 2
}

fn expected_active() -> i64 {
    (0..N).filter(|i| active_for(*i) == 1).count() as i64
}

fn jsonb_rows_values() -> String {
    // The GIN index covers `data` only, so keep the JSON small (GIN terms must
    // stay under the 64-byte index-key limit). The split-forcing padding lives
    // in a separate, non-indexed `filler` column.
    let filler = "x".repeat(PAD);
    let mut out = String::new();
    for i in 0..N {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!(
            "({i}, CAST('{{\"id\":{i},\"active\":{a}}}' AS JSONB), '{filler}')",
            a = active_for(i)
        ));
    }
    out
}

fn btree_rows_values() -> String {
    let pad = "x".repeat(PAD);
    let mut out = String::new();
    for i in 0..N {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&format!("({i}, {a}, '{pad}')", a = active_for(i)));
    }
    out
}

// ── tests ──────────────────────────────────────────────────────────────────────

/// `CREATE INDEX … USING GIN` over an already-populated clustered table must
/// index every existing row, so a later `@>` query (which the planner routes to
/// the GIN index) returns the correct count. Before the fix it returned 0.
#[test]
fn gin_bulk_build_over_existing_clustered_rows_indexes_all() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_deferred();
    let expected = expected_active();

    run_drv(
        "CREATE TABLE t (id INT NOT NULL, data JSONB NOT NULL, filler TEXT NOT NULL, PRIMARY KEY(id))",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Open an explicit transaction and load the data (uncommitted). This is the
    // pymysql default (autocommit off): rows are not committed until the DDL
    // implicitly commits the transaction.
    run_drv("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_drv(
        &format!("INSERT INTO t VALUES {}", jsonb_rows_values()),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Seq scan within the transaction sees the connection's own writes.
    let pre = scalar_count(
        run_drv(
            "SELECT COUNT(*) FROM t WHERE data @> '{\"active\":1}'",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        pre, expected,
        "pre-index seq scan must see all active=1 rows"
    );

    // Build the GIN index over the existing rows (DDL implicitly commits).
    run_drv(
        "CREATE INDEX gt ON t USING GIN (data)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // The `@>` predicate is planned as a GIN scan now that the index exists, so
    // this count reflects the GIN index contents — 0 if the build saw no rows.
    let explain = common::rows(
        run_drv(
            "EXPLAIN SELECT id FROM t WHERE data @> '{\"active\":1}'",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        explain[0][3],
        Value::Text("gin".into()),
        "the @> query must use the GIN index (else the count is not a GIN-path assertion); plan row = {:?}",
        explain[0]
    );

    let via_gin = scalar_count(
        run_drv(
            "SELECT COUNT(*) FROM t WHERE data @> '{\"active\":1}'",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(
        via_gin, expected,
        "GIN bulk build over existing clustered rows must index all matching rows"
    );
}

/// A plain B-tree secondary index built over an already-populated clustered
/// table must contain one entry per existing row (same bulk-scan path as GIN).
/// Before the fix the secondary index was empty.
#[test]
fn btree_secondary_bulk_build_over_existing_clustered_rows_indexes_all() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_deferred();

    run_drv(
        "CREATE TABLE t (id INT PRIMARY KEY, bucket INT NOT NULL, pad TEXT NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_drv("BEGIN", &mut storage, &mut txn, &mut bloom, &mut ctx).unwrap();
    run_drv(
        &format!("INSERT INTO t VALUES {}", btree_rows_values()),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    run_drv(
        "CREATE INDEX idx_bucket ON t (bucket)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // The secondary index must hold exactly one entry per existing row.
    let indexes = table_indexes(&storage, &txn, "t");
    let entries = secondary_entry_count(&storage, &indexes, "idx_bucket");
    assert_eq!(
        entries, N as usize,
        "B-tree secondary bulk build must index every existing clustered row"
    );

    // And a point query on the indexed column returns the right count.
    let expected_bucket1 = expected_active();
    let got = scalar_count(
        run_drv(
            "SELECT COUNT(*) FROM t WHERE bucket = 1",
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(got, expected_bucket1, "secondary index point query count");
}

/// ALTER TABLE MODIFY COLUMN on a clustered table rewrites every row and then
/// rebuilds any secondary index that depends on the column, via the same
/// clustered bulk-scan (`build_index_root_from_clustered`). With enough rows to
/// span multiple clustered leaves, the rebuild must still index every row — i.e.
/// it must scan the live multi-leaf root, not a stale one.
#[test]
fn modify_column_rebuild_over_multileaf_clustered_table_indexes_all() {
    let (mut storage, mut txn, mut bloom, mut ctx) = setup_deferred();

    run_drv(
        "CREATE TABLE t (id INT PRIMARY KEY, score INT NOT NULL, pad TEXT NOT NULL)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();
    run_drv(
        "CREATE INDEX idx_score ON t (score)",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Distinct scores, padded rows → several clustered leaf splits.
    let filler = "x".repeat(PAD);
    let mut values = String::new();
    for i in 0..N {
        if i > 0 {
            values.push_str(", ");
        }
        values.push_str(&format!("({i}, {i}, '{filler}')"));
    }
    run_drv(
        &format!("INSERT INTO t VALUES {values}"),
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // Sanity: incremental maintenance indexed every row before the rebuild.
    let indexes_before = table_indexes(&storage, &txn, "t");
    let before = secondary_entry_count(&storage, &indexes_before, "idx_score");
    assert_eq!(before, N as usize, "pre-ALTER secondary entry count");

    // Type change rewrites all rows and rebuilds idx_score (depends on `score`).
    run_drv(
        "ALTER TABLE t MODIFY COLUMN score BIGINT",
        &mut storage,
        &mut txn,
        &mut bloom,
        &mut ctx,
    )
    .unwrap();

    // The rebuilt index must still contain one entry per row.
    let indexes_after = table_indexes(&storage, &txn, "t");
    let after = secondary_entry_count(&storage, &indexes_after, "idx_score");
    assert_eq!(
        after, N as usize,
        "ALTER rebuild over a multi-leaf clustered table must index every row"
    );

    // And a point lookup on the rebuilt index finds the (widened) row.
    let probe = N / 2;
    let got = common::rows(
        run_drv(
            &format!("SELECT id FROM t WHERE score = {probe}"),
            &mut storage,
            &mut txn,
            &mut bloom,
            &mut ctx,
        )
        .unwrap(),
    );
    assert_eq!(got.len(), 1, "rebuilt index point lookup must find the row");
    assert_eq!(got[0][0], Value::Int(probe as i32));
}
