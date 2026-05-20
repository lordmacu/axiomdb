# Plan: deferred-fsync — session-level SET synchronous

Phase: perf-sqlite-gap
Task: Attack 6 — expose `SET synchronous = STRICT|NORMAL|OFF` and make
the bench apples-to-apples
Spec: specs/fase-perf-sqlite-gap/spec-deferred-fsync.md
Status: done

## Summary

Five TDD-ordered steps. **6.1** adds a `durability_override:
Option<WalDurabilityPolicy>` slot on `ConnectionTxn` + makes `commit()`
honor it (storage layer; no behavior change with `None`). **6.2** adds
`SessionContext.synchronous`, `parse_synchronous_setting`, and the
`SET synchronous = '<value>'` handler. **6.3** wires the SQL autocommit
path (`exec_with_ctx.rs`) to inject the session's value into every
newly-created `ConnectionTxn`. **6.4** updates the comparison bench
to issue `SET synchronous = 'NORMAL'` on the AxiomDB connection so the
SQLite-vs-AxiomDB numbers are apples-to-apples. **6.5** closes with
measurements + docs + memory.

Order: storage-layer first (lowest risk, pure addition), SQL session
second, executor wiring third, bench fourth, close fifth. Each step's
commit is independently revertible.

## Dependencies

Must be done first:
- [x] spec-deferred-fsync approved (commit `be583f27`)
- [x] `WalDurabilityPolicy::{Strict, Normal, Off}` already exists
      ([`txn.rs:273`](crates/axiomdb-wal/src/txn.rs))
- [x] Per-policy commit branching at
      [`txn_begin_commit.rs:111-127`](crates/axiomdb-wal/src/txn_begin_commit.rs)
- [x] Existing `SET name = value` parser + dispatcher at
      [`exec_dispatch.rs:797`](crates/axiomdb-sql/src/executor/exec_dispatch.rs)

Blocks (until done):
- Future: `SELECT @@synchronous` (read-current-value)
- Future: PRAGMA parser (SQLite-syntax alias for SET)
- Future: `axiomdb.toml` `[durability] default = "normal"`

## Affected files

New files:
- `crates/axiomdb-sql/tests/integration_set_synchronous.rs` — 11
  spec edge-case tests

Modified files:
- `crates/axiomdb-wal/src/txn.rs` — add `pub durability_override:
  Option<WalDurabilityPolicy>` to `ConnectionTxn`
- `crates/axiomdb-wal/src/txn_begin_commit.rs` — init the field in
  `begin()`; honor it in `commit()`
- `crates/axiomdb-wal/src/txn_inspect.rs` — init the field in test
  fixtures
- `crates/axiomdb-sql/src/session.rs` — add `SessionDurability` enum +
  `synchronous: SessionDurability` field + `synchronous()` /
  `set_synchronous()` methods + `parse_synchronous_setting`
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — extend
  `execute_set_ctx` to handle `"synchronous"`
- `crates/axiomdb-sql/src/executor/exec_with_ctx.rs` — after every
  `txn.begin()` / `txn.begin_with_isolation()` in the autocommit
  / implicit-txn path, set `conn_txn.durability_override` from
  `ctx.synchronous`
- `benches/comparison/axiomdb_bench/src/main.rs` — issue
  `SET synchronous = 'NORMAL'` at Db open (mirror SQLite's
  `PRAGMA synchronous=NORMAL`)
- `docs/perf-sqlite-gap.md` — Step 6.5 update
- `memory/project_sqlite_baseline.md` — Step 6.5 update

---

## Step 6.1 — `ConnectionTxn.durability_override` + `commit()` honors it

**Goal:** Per-transaction durability override at the storage layer. No
caller uses it yet (`None` everywhere).

**Files:**
- `crates/axiomdb-wal/src/txn.rs` — add the field
- `crates/axiomdb-wal/src/txn_begin_commit.rs` — init + use
- `crates/axiomdb-wal/src/txn_inspect.rs` — init for tests
- `crates/axiomdb-wal/tests/integration_durability.rs` (or similar
  existing test file) — 2 unit tests

**Approach:** TDD — write the override tests, then the field + commit
honoring.

### Tests to add

```rust
// In an existing axiomdb-wal test file.

#[test]
fn commit_with_override_normal_skips_fsync() {
    // Use a mock filesystem or a counter on the WAL writer to assert
    // that sync_all was NOT called when ConnectionTxn.durability_override
    // is Some(Normal). Existing test infra likely has a helper for this.
    let mgr = TxnManager::create(tmp_wal_path()).unwrap();
    let mut conn = mgr.begin().unwrap();
    conn.durability_override = Some(WalDurabilityPolicy::Normal);
    // ...write something so undo_ops is non-empty...
    mgr.commit(conn).unwrap();
    assert!(/* fsync was not called */);
}

#[test]
fn commit_with_no_override_falls_back_to_instance_policy() {
    // Default instance policy is Strict → fsync.
    let mgr = TxnManager::create(tmp_wal_path()).unwrap();
    let mut conn = mgr.begin().unwrap();
    // durability_override stays None.
    // ...write...
    mgr.commit(conn).unwrap();
    assert!(/* fsync was called */);
}
```

### Implementation outline

```rust
// crates/axiomdb-wal/src/txn.rs

pub struct ConnectionTxn {
    // ... existing fields ...

    /// Attack 6: per-transaction override of the WAL durability policy.
    /// When `Some`, replaces `TxnManager.durability_policy` at commit
    /// time. Used by the SQL layer to honor `SET synchronous = '...'`
    /// without mutating the instance-wide default. `None` = use the
    /// instance default.
    pub durability_override: Option<WalDurabilityPolicy>,
}
```

```rust
// crates/axiomdb-wal/src/txn_begin_commit.rs — in begin()
Ok(ConnectionTxn {
    // ... existing fields ...
    durability_override: None,
})
```

```rust
// crates/axiomdb-wal/src/txn_begin_commit.rs — in commit(), replace
// `match self.durability_policy {` with:
let effective_policy = conn_txn
    .durability_override
    .unwrap_or(self.durability_policy);
match effective_policy {
    WalDurabilityPolicy::Strict => { ... }
    WalDurabilityPolicy::Normal => { ... }
    WalDurabilityPolicy::Off => { ... }
}
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-wal
./tools/vm.sh clippy axiomdb-wal 2>&1 | tail -5
```

### Commit

```
feat(perf-sqlite-gap): step 6.1 — ConnectionTxn.durability_override

Adds a per-transaction durability override at the WAL layer. When set,
replaces TxnManager.durability_policy for that commit's fsync decision.
Default = None (use instance policy). No callers wired yet.

Enables Attack 6's session-level SET synchronous without mutating the
instance-wide default. Mirrors SQLite's per-pager safety_level
(research/sqlite/src/pager.c:3590).

2 unit tests: override Normal skips fsync, no-override falls back to
instance default.
```

---

## Step 6.2 — `SessionDurability` + `SET synchronous` parser/handler

**Goal:** SQL layer accepts `SET synchronous = 'NORMAL' | 'STRICT' |
'OFF' | DEFAULT`, parses + validates, updates `SessionContext`.

**Files:**
- `crates/axiomdb-sql/src/session.rs` — enum, field, parser, methods
- `crates/axiomdb-sql/src/executor/exec_dispatch.rs` — extend
  `execute_set_ctx`'s match
- `crates/axiomdb-sql/tests/integration_set_synchronous.rs` (new) — 7
  unit-level tests

**Approach:** TDD — write the parser + SET handler tests, then the impl.

### Tests to add

```rust
// crates/axiomdb-sql/tests/integration_set_synchronous.rs
use axiomdb_sql::session::{parse_synchronous_setting, SessionDurability};

#[test]
fn parse_synchronous_accepts_canonical_names() {
    assert_eq!(parse_synchronous_setting("STRICT").unwrap(), SessionDurability::Strict);
    assert_eq!(parse_synchronous_setting("NORMAL").unwrap(), SessionDurability::Normal);
    assert_eq!(parse_synchronous_setting("OFF").unwrap(), SessionDurability::Off);
}

#[test]
fn parse_synchronous_case_insensitive() {
    assert_eq!(parse_synchronous_setting("normal").unwrap(), SessionDurability::Normal);
    assert_eq!(parse_synchronous_setting("Normal").unwrap(), SessionDurability::Normal);
    assert_eq!(parse_synchronous_setting("nOrMaL").unwrap(), SessionDurability::Normal);
}

#[test]
fn parse_synchronous_accepts_sqlite_aliases() {
    // SQLite uses 5 levels; our 3 map per spec.
    assert_eq!(parse_synchronous_setting("FULL").unwrap(),  SessionDurability::Strict);
    assert_eq!(parse_synchronous_setting("EXTRA").unwrap(), SessionDurability::Strict);
}

#[test]
fn parse_synchronous_accepts_numeric_forms() {
    // SQLite getSafetyLevel: 0=OFF, 1=ON(=NORMAL legacy), 2=NORMAL,
    // 3=FULL, 4=EXTRA. Our mapping per the spec.
    assert_eq!(parse_synchronous_setting("0").unwrap(), SessionDurability::Off);
    assert_eq!(parse_synchronous_setting("1").unwrap(), SessionDurability::Normal);
    assert_eq!(parse_synchronous_setting("2").unwrap(), SessionDurability::Normal);
    assert_eq!(parse_synchronous_setting("3").unwrap(), SessionDurability::Strict);
    assert_eq!(parse_synchronous_setting("4").unwrap(), SessionDurability::Strict);
}

#[test]
fn parse_synchronous_rejects_garbage() {
    let err = parse_synchronous_setting("banana").unwrap_err();
    assert!(matches!(err, axiomdb_core::error::DbError::InvalidValue { .. }));
}

#[test]
fn session_context_default_is_strict() {
    let ctx = axiomdb_sql::SessionContext::default();
    assert_eq!(ctx.synchronous(), SessionDurability::Strict);
}

#[test]
fn session_context_set_synchronous_updates_value() {
    let mut ctx = axiomdb_sql::SessionContext::default();
    ctx.set_synchronous(SessionDurability::Normal);
    assert_eq!(ctx.synchronous(), SessionDurability::Normal);
    ctx.set_synchronous(SessionDurability::Strict);
    assert_eq!(ctx.synchronous(), SessionDurability::Strict);
}
```

### Implementation outline

```rust
// crates/axiomdb-sql/src/session.rs

/// Session-level durability mode. Maps to axiomdb_wal::WalDurabilityPolicy.
/// Mirrors SQLite's PRAGMA synchronous
/// (research/sqlite/src/pager.c:3590-3611).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionDurability {
    /// **Default.** fsync per commit (SQLite synchronous=FULL).
    #[default]
    Strict,
    /// Flush only, no fsync per commit (SQLite synchronous=NORMAL WAL).
    Normal,
    /// No flush, no fsync (SQLite synchronous=OFF).
    Off,
}

impl SessionDurability {
    /// Maps to the WAL crate's enum.
    pub fn to_wal_policy(self) -> axiomdb_wal::WalDurabilityPolicy {
        match self {
            Self::Strict => axiomdb_wal::WalDurabilityPolicy::Strict,
            Self::Normal => axiomdb_wal::WalDurabilityPolicy::Normal,
            Self::Off    => axiomdb_wal::WalDurabilityPolicy::Off,
        }
    }
}

/// Parses `SET synchronous = '<value>'`. Accepts canonical names,
/// case-insensitive, plus SQLite aliases (FULL → Strict, EXTRA → Strict)
/// and numeric forms (0=Off, 1/2=Normal, 3/4=Strict).
pub fn parse_synchronous_setting(raw: &str) -> Result<SessionDurability, DbError> {
    let s = raw.trim().trim_matches('\'').trim_matches('"').to_ascii_lowercase();
    match s.as_str() {
        "off"   | "0"             => Ok(SessionDurability::Off),
        "normal" | "1" | "2" | "on" => Ok(SessionDurability::Normal),
        "strict" | "full" | "extra" | "3" | "4" => Ok(SessionDurability::Strict),
        _ => Err(DbError::InvalidValue {
            reason: format!(
                "invalid synchronous value '{raw}'; expected \
                 STRICT | NORMAL | OFF | FULL | EXTRA | 0..4 | DEFAULT"
            ),
        }),
    }
}

impl SessionContext {
    pub fn synchronous(&self) -> SessionDurability { self.synchronous }
    pub fn set_synchronous(&mut self, mode: SessionDurability) {
        self.synchronous = mode;
    }
}

pub struct SessionContext {
    // ... existing fields ...
    synchronous: SessionDurability, // default = Strict via #[derive(Default)]
}
```

```rust
// crates/axiomdb-sql/src/executor/exec_dispatch.rs — add to the match
"synchronous" => match stmt.value {
    SetValue::Default => ctx.set_synchronous(SessionDurability::default()),
    SetValue::Expr(expr) => {
        // Mirror SQLite pragma.c:1136-1138 — reject inside a transaction.
        if ctx.in_explicit_txn {
            return Err(DbError::InvalidValue {
                reason: "synchronous cannot be changed inside a transaction".into(),
            });
        }
        let v = eval(&expr, &[])?;
        let raw = match &v {
            Value::Text(s) => s.clone(),
            Value::Int(n)  => n.to_string(),
            Value::BigInt(n) => n.to_string(),
            other => return Err(DbError::InvalidValue {
                reason: format!("synchronous: unsupported value type {other:?}"),
            }),
        };
        let mode = crate::session::parse_synchronous_setting(&raw)?;
        ctx.set_synchronous(mode);
    }
},
```

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_set_synchronous
./tools/vm.sh test -p axiomdb-sql       # broad
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
```

### Commit

```
feat(perf-sqlite-gap): step 6.2 — SET synchronous = STRICT/NORMAL/OFF

Adds SessionDurability enum + parse_synchronous_setting +
SessionContext.synchronous field + SET handler that mirrors SQLite's
PRAGMA synchronous semantics (case-insensitive, SQLite alias names,
numeric forms, in-txn rejection).

Default = Strict (no durability regression). 7 unit tests cover
canonical names, case, aliases (FULL/EXTRA → Strict), numeric forms,
invalid input, default, set/get.

No wiring to commit() yet — Step 6.3 does that.
```

---

## Step 6.3 — Wire `ctx.synchronous` → `ConnectionTxn.durability_override`

**Goal:** Every implicit/autocommit `txn.begin()` in
`exec_with_ctx.rs` injects the session's synchronous into the new
ConnectionTxn so commit() uses it.

**Files:**
- `crates/axiomdb-sql/src/executor/exec_with_ctx.rs` — at every
  `txn.begin()` / `txn.begin_with_isolation()` call (lines 139, 234,
  255, 284, 302, 333, 349 per the earlier grep), set
  `conn_txn.durability_override = Some(ctx.synchronous.to_wal_policy())`
- `crates/axiomdb-sql/tests/integration_set_synchronous.rs` — 3
  end-to-end tests

**Approach:** TDD — end-to-end test that SET NORMAL + INSERT issues no
fsync (or just measures faster wall-clock to assert ≥ 5× speedup).

### Tests to add

```rust
mod harness {
    use axiomdb_catalog::bootstrap::CatalogBootstrap;
    use axiomdb_core::error::DbError;
    use axiomdb_sql::{
        analyze_cached, bloom::BloomRegistry, execute_with_ctx,
        parse_with_sql_mode, result::QueryResult, SchemaCache, SessionContext,
    };
    use axiomdb_storage::MemoryStorage;
    use axiomdb_wal::TxnManager;

    pub struct Harness { /* same as integration_cursor_reuse harness */ }
    impl Harness { pub fn new() -> Self { ... }; pub fn run(&mut self, sql: &str) -> Result<QueryResult, DbError> { ... } }
}

#[test]
fn end_to_end_set_synchronous_updates_session() {
    let mut h = harness::Harness::new();
    h.run("SET synchronous = 'NORMAL'").unwrap();
    assert_eq!(h.session.synchronous(), SessionDurability::Normal);
}

#[test]
fn set_synchronous_rejected_inside_transaction() {
    let mut h = harness::Harness::new();
    h.run("BEGIN").unwrap();
    let err = h.run("SET synchronous = 'NORMAL'").unwrap_err();
    assert!(matches!(err, DbError::InvalidValue { ref reason } if reason.contains("inside a transaction")));
    // Value unchanged after rejection.
    assert_eq!(h.session.synchronous(), SessionDurability::Strict);
}

#[test]
fn autocommit_insert_with_normal_synchronous_is_significantly_faster() {
    // Wall-clock smoke: 200 autocommit INSERTs with STRICT then with NORMAL;
    // NORMAL must be at least 3× faster (sanity check; tighter assertion
    // in the bench).
    fn time_inserts(mode: &str) -> std::time::Duration {
        let mut h = harness::Harness::new();
        h.run("CREATE TABLE t (id INT PRIMARY KEY, v INT)").unwrap();
        h.run(&format!("SET synchronous = '{mode}'")).unwrap();
        let t0 = std::time::Instant::now();
        for i in 1..=200 {
            h.run(&format!("INSERT INTO t VALUES ({i}, 0)")).unwrap();
        }
        t0.elapsed()
    }
    let strict = time_inserts("STRICT");
    let normal = time_inserts("NORMAL");
    assert!(
        normal.as_secs_f64() * 3.0 < strict.as_secs_f64(),
        "NORMAL must be ≥ 3× faster than STRICT — got strict={:?} normal={:?}",
        strict, normal,
    );
}
```

### Implementation outline

For each `txn.begin()` call in exec_with_ctx.rs (7 sites per the
earlier grep), wrap:

```rust
// Before:
ctx.conn_txn = Some(txn.begin()?);

// After:
let mut conn = txn.begin()?;
conn.durability_override = Some(ctx.synchronous().to_wal_policy());
ctx.conn_txn = Some(conn);
```

Same pattern for `txn.begin_with_isolation(level)?`.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql --test integration_set_synchronous
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh clippy axiomdb-sql 2>&1 | tail -5
```

### Commit

```
perf(perf-sqlite-gap): step 6.3 — wire SET synchronous → ConnectionTxn

Every implicit/autocommit `txn.begin()` in exec_with_ctx.rs now sets
ConnectionTxn.durability_override from SessionContext.synchronous, so
the WAL commit honors the session's synchronous setting.

3 end-to-end tests: SET updates session, SET inside txn rejected,
autocommit INSERT with NORMAL is ≥ 3× faster than STRICT (sanity
check before the full bench in Step 6.5).
```

---

## Step 6.4 — Bench fairness fix

**Goal:** `axiomdb_bench --compare` issues `SET synchronous = 'NORMAL'`
on the AxiomDB connection so it matches SQLite's
`PRAGMA synchronous=NORMAL` (which the bench has been setting since
day one).

**Files:**
- `benches/comparison/axiomdb_bench/src/main.rs` — after `Db::open(...)`
  in the `--compare` setup, issue `db.execute("SET synchronous = 'NORMAL'")`
- `benches/sqlite_vs_axiomdb/bench.py` — same for the Python harness
- `benches/sqlite_vs_axiomdb/README.md` — document the durability
  config alignment

### Implementation outline

```rust
// In axiomdb_bench/src/main.rs setup_axiomdb or wherever Db::open is called:
let mut db = Db::open(...)?;
db.execute("SET synchronous = 'NORMAL'")?; // match SQLite's bench config
```

### Verification

```bash
cargo build --release -p axiomdb-bench-comparison
for i in 1 2 3; do
    cargo run -p axiomdb-bench-comparison --release -- \
        --compare --rows 10000 2>&1 | grep -E "insert_autocommit|insert_batch"
done
# Expect insert_autocommit ≥ 60K rows/s.
```

### Commit

```
bench(perf-sqlite-gap): step 6.4 — apples-to-apples synchronous config

axiomdb_bench --compare now issues SET synchronous = 'NORMAL' on the
AxiomDB connection at open, matching the PRAGMA synchronous=NORMAL the
bench has always set on SQLite. The two engines are now running with
equivalent durability semantics (both flush-no-fsync per commit).

Results (--compare --rows 10000, 3-run median):
  insert_autocommit:  8.7K → ?K rows/s
  insert_batch:       21K → ?K rows/s
  Gap vs SQLite:      15× → ?× (insert_autocommit)

Same change for the Python bench (benches/sqlite_vs_axiomdb/bench.py)
+ doc update in its README.
```

---

## Step 6.5 — Measure + close

**Goal:** Verify every spec done-criterion; update docs + memory.

### Verification against spec

- [ ] `SET synchronous = 'NORMAL' | 'STRICT' | 'OFF' | DEFAULT` parses
      and updates `SessionContext.synchronous`.
- [ ] Default stays `SessionDurability::Strict`.
- [ ] Numeric + alias forms accepted.
- [ ] Inside-txn rejection works (covered by Step 6.3 test).
- [ ] `axiomdb_bench --compare --rows 10000` shows `insert_autocommit
      ≥ 60K rows/s` (after Step 6.4 bench update).
- [ ] `cargo nextest run --workspace` (Lima) — clean.
- [ ] `cargo clippy --workspace -- -D warnings` (Lima) — clean.
- [ ] `cargo fmt --check` — clean.
- [ ] `tools/wire-test.py` — clean (pre-flight per memory).
- [ ] 11+ tests in `integration_set_synchronous.rs` covering every
      spec edge case (7 unit + 3 end-to-end + 1 cross-session).

### Docs to update

- `docs/perf-sqlite-gap.md` — new "Attack 6" section with the
  durability tradeoff, the bench-fairness fix, before/after numbers.
- `memory/project_sqlite_baseline.md` — append post-Attack-6 row.

### Final commit

```
feat(perf-sqlite-gap): close Attack 6 — SET synchronous + bench fairness

Implements specs/fase-perf-sqlite-gap/spec-deferred-fsync.md
Plan: specs/fase-perf-sqlite-gap/plan-deferred-fsync.md

Results (axiomdb_bench --compare --rows 10000):
  insert_autocommit:  8.7K → ?K rows/s
  insert_batch:       21K → ?K rows/s
  Gap vs SQLite:      15× → ?×

Tests: 11+ integration tests covering all spec edge cases.
All 4351+ workspace tests pass, clippy + fmt clean, wire smoke green.
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `WalDurabilityPolicy::Normal` not battle-tested under crash | Medium | Default stays Strict; opt-in only. Document the durability tradeoff loudly in docs/perf-sqlite-gap.md. Future crash-recovery test suite (Phase 19) will exercise NORMAL. |
| Bench numbers look "too good" without context | Low | Step 6.4 commit message + docs explicitly call out the durability config. Add a `--compare` header noting "both engines: synchronous=NORMAL". |
| Multiple `txn.begin()` sites in exec_with_ctx.rs missed | Medium | Grep audit in Step 6.3; 7 sites known; iterate until grep returns zero callers that don't set the override. |
| User sets NORMAL in production and loses data on crash | Low | PRAGMA-style opt-in; default Strict; docs make the tradeoff explicit. |
| In-txn rejection breaks an existing test that does SET synchronous mid-txn | Low | No existing test references synchronous (new SQL surface). |
| Wire-protocol can't parse SET synchronous | Low | Reuses the existing SET dispatcher; no parser change. |

## Rollback plan

1. Each step has its own commit, individually revertible:
   - Step 6.1 is pure addition — revert with zero impact.
   - Step 6.2 adds parser + handler — revert removes the SQL surface.
   - Step 6.3 wires the executor — revert restores Strict-default
     behavior for all sessions.
   - Step 6.4 is bench-only — revert just changes bench config.
2. If the whole attack is abandoned:
   `git branch abandoned/plan-deferred-fsync-2026-05-17` from the last
   clean commit; revert spec to `draft` with a note.

## Estimated effort

Total: ~1.5 days.
- Step 6.1 (storage override + 2 tests): 0.5 day
- Step 6.2 (session enum + parser + SET handler + 7 tests): 0.5 day
- Step 6.3 (executor wiring + 3 end-to-end tests): 0.3 day
- Step 6.4 (bench fairness): 0.1 day
- Step 6.5 (measure + docs + close): 0.1 day
