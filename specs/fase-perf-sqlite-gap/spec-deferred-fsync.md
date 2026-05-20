# Spec: deferred-fsync — session-level `SET synchronous`

Phase: perf-sqlite-gap — close embedded gap with SQLite
Task: Attack 6 — expose per-session durability mode via `SET synchronous`;
make the comparison bench apples-to-apples
Status: implemented

## Context

While closing Attack 5 we discovered a fundamental unfairness in the
bench:

- `benches/comparison/axiomdb_bench/src/main.rs:464` configures SQLite
  with `PRAGMA synchronous=NORMAL` — flush-to-OS-cache only, NO
  fsync per commit.
- AxiomDB uses its hard-coded default: `WalDurabilityPolicy::Strict`
  ([`txn_construction.rs:17`](crates/axiomdb-wal/src/txn_construction.rs))
  — `sync_all()` per commit.

The infrastructure for non-fsync commits already exists in our WAL:
`WalDurabilityPolicy::{Strict, Normal, Off}` plus the deferred-fsync
`FsyncPipeline` (Phase 40). What's missing is the SQL-layer plumbing
to let a session opt into `Normal` mode.

Once exposed, the comparison becomes apples-to-apples and
`insert_autocommit` should jump from 8.9K to ~50-100K rows/s simply
because we stop waiting for fsync on every commit.

SQLite's analog: `PRAGMA synchronous` (`pragma.c:1132-1148`,
`pager.c:3590-3611`). They expose 5 levels (OFF, ON, NORMAL, FULL,
EXTRA); for WAL mode the meaningful ones are OFF / NORMAL / FULL.

## Goal

Add a session-level setting `synchronous` (settable via `SET
synchronous = '<value>'`) that maps to AxiomDB's existing
`WalDurabilityPolicy` and applies on the next commit, so users can
explicitly trade durability-on-ACK for throughput.

## Non-goals

- **Per-database / per-schema durability** (SQLite's
  `pDb->safety_level`). Out of scope. We're a single-database server;
  per-session is enough.
- **Persistent default in `axiomdb.toml`**. Out of scope — config is
  already read at `Db::open` for the per-instance default; session-level
  override goes through SQL.
- **PRAGMA parser** (SQLite's exact syntax). We already have `SET name
  = value`; adding `PRAGMA` is a separate task. Document `SET
  synchronous` as the AxiomDB analog of `PRAGMA synchronous`.
- **Changing the default to NORMAL**. The default stays `Strict` — full
  durability out of the box. Users opt into looser modes explicitly.
  (SQLite also defaults to FULL.)
- **Async / background fsync (`FsyncPipeline`)**. The infrastructure
  exists for this (`ConnectionTxn.deferred_commit_mode`) but plumbing
  the toggle through is a follow-up; Attack 6 only exposes
  Strict/Normal/Off.
- **Cross-connection durability hint propagation**. Each session
  owns its own setting; sessions don't communicate.

## Behavior

### Public API

New SQL surface (no Rust API changes):

```sql
-- Set:
SET synchronous = 'STRICT';   -- default; fsync per commit (durable on ACK)
SET synchronous = 'NORMAL';   -- flush only; durable on commit ORDERING
SET synchronous = 'OFF';      -- no flush, no fsync (worst case)
SET synchronous = DEFAULT;    -- reset to instance default (Strict)

-- Read (Phase 6 follow-up — `SELECT @@synchronous`):
-- NOT in scope of Attack 6; tracked separately.
```

Equivalent forms accepted (SQLite alias compatibility):
- `'FULL'`, `'EXTRA'`, `'2'`, `'3'`, `'4'` → STRICT
- `'NORMAL'`, `'1'` → NORMAL  *(SQLite uses `1=ON=NORMAL` historically)*
- `'OFF'`, `'0'` → OFF

Internal additions:

```rust
// crates/axiomdb-sql/src/session.rs

/// Session-level durability mode. Maps to axiomdb_wal::WalDurabilityPolicy.
/// Mirrors SQLite's PRAGMA synchronous (research/sqlite/src/pager.c:3590).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionDurability {
    /// **Default.** Full fsync per commit. Equivalent to SQLite
    /// `synchronous=FULL`. Durable on commit ACK.
    #[default]
    Strict,
    /// Flush to OS page cache only; no fsync per commit. Equivalent to
    /// SQLite `synchronous=NORMAL` in WAL mode. Durable in COMMIT
    /// ORDERING — recent commits may be lost on crash but the DB
    /// remains internally consistent.
    Normal,
    /// No flush, no fsync. Equivalent to SQLite `synchronous=OFF`.
    /// Data loss possible on crash. Intended for ephemeral / test DBs.
    Off,
}

impl SessionContext {
    pub fn synchronous(&self) -> SessionDurability { ... }
    pub fn set_synchronous(&mut self, mode: SessionDurability) { ... }
}

pub fn parse_synchronous_setting(raw: &str) -> Result<SessionDurability, DbError>;
```

```rust
// crates/axiomdb-wal/src/txn.rs (or txn_inspect.rs)

impl TxnManager {
    /// Live setter (already exists for boot-time config; if not, add it).
    /// Affects subsequent commits only; in-flight transactions keep
    /// the policy they observed at BEGIN time (snapshotted into
    /// ConnectionTxn).
    pub fn set_durability_policy(&self, policy: WalDurabilityPolicy);
}
```

### Semantics

`SET synchronous = '<value>'`:

1. **Preconditions:**
   - Caller is in a session.
   - Caller is NOT inside an explicit transaction (mirrors SQLite's
     `!db->autoCommit` check at `pragma.c:1136`). Returns
     `DbError::InvalidValue { reason: "synchronous cannot be changed
     inside a transaction" }` otherwise.

2. **Side effects:**
   - Updates `SessionContext.synchronous` to the parsed value.
   - On the NEXT call to `execute_with_ctx`, when a commit fires, the
     session's policy is read and applied to the `TxnManager`'s commit
     code path.

3. **No on-disk effect.** The setting is purely runtime; restarting
   the `Db` resets to the instance default (Strict).

4. **No cross-session effect.** Each `SessionContext` owns its own
   value.

`SET synchronous = DEFAULT` resets the session to the instance default.

Reading the current value via `SELECT @@synchronous` is **out of
scope** (Phase 6 follow-up to keep this spec small).

### Commit-time mapping

```rust
// In Db::run_inner or wherever the implicit/explicit commit fires:
match ctx.synchronous() {
    SessionDurability::Strict => /* current path: sync_all() */,
    SessionDurability::Normal => /* flush_no_sync() — already implemented
                                    behind WalDurabilityPolicy::Normal */,
    SessionDurability::Off    => /* no flush — WalDurabilityPolicy::Off */,
}
```

The cleanest wiring: pass the current session policy into
`TxnManager::commit` as a parameter (per-commit override). Or: set the
`TxnManager` policy at the start of each statement and reset it after.
**Implementation chooses; this spec only fixes the behavior.**

### Error cases

| Input | Expected error | Message |
|-------|----------------|---------|
| `SET synchronous = 'foo'` | `DbError::InvalidValue` | `"invalid synchronous value 'foo'; expected STRICT | NORMAL | OFF | FULL | EXTRA | 0..4 | DEFAULT"` |
| `SET synchronous = 'NORMAL'` while inside `BEGIN..COMMIT` | `DbError::InvalidValue` | `"synchronous cannot be changed inside a transaction"` |
| `SET synchronous` (no value) | `DbError::ParseError` | (parser's existing message — `SET` requires `=`) |

## Edge cases

Each becomes a test case in the plan:

- [ ] `SET synchronous = 'NORMAL'` updates session; subsequent INSERTs
      complete without fsync (verifiable via faster wall-clock or via a
      mock that counts `sync_all` calls).
- [ ] `SET synchronous = 'STRICT'` (back to default) restores fsync
      per commit.
- [ ] `SET synchronous = DEFAULT` resets to the instance default.
- [ ] `SET synchronous = 'OFF'` works (flush + fsync skipped both).
- [ ] Case-insensitive: `'normal'` == `'NORMAL'` == `'Normal'`.
- [ ] Numeric forms: `0` → OFF, `1` → NORMAL, `2..4` → STRICT.
- [ ] Invalid value (`'foo'`) returns `DbError::InvalidValue`.
- [ ] Inside an explicit transaction the SET returns
      `DbError::InvalidValue` and DOES NOT change the value.
- [ ] Cross-session isolation: session A's `SET synchronous = NORMAL`
      does NOT affect session B's value.
- [ ] After a `Db::close` + `Db::open`, the setting resets to the
      instance default (no persistence).
- [ ] `SET synchronous` does NOT affect read-only statements (already
      `flush_no_sync` per existing code).
- [ ] Wire-protocol equivalent (`pymysql`) can issue the SET and
      observe the speedup.

## On-disk format

No on-disk format change. The setting is purely in-memory per session.

## Performance budget

Measured via `axiomdb_bench --compare --rows 10000` (3 runs, median),
WITH the bench updated to issue `SET synchronous = 'NORMAL'` on the
AxiomDB connection at startup (matching SQLite's existing
`synchronous=NORMAL` config).

| Scenario | Today (Strict default) | After Attack 6 (NORMAL via SET) |
|----------|-----------------------:|--------------------------------:|
| insert_autocommit | 8.9K rows/s | **≥ 60K rows/s** (≥ 6.5× — fsync removed) |
| insert_batch (1 fsync per txn) | 21K rows/s | ≥ 30K rows/s (~1.4× — flush vs fsync per commit) |
| crud_flow/insert | 21K rows/s | ≥ 30K rows/s |
| point_lookup | 8.8K ops/s | unchanged (reads already flush_no_sync) |
| range_scan, full_scan, count_star, group_by | unchanged | unchanged |
| Workspace test runtime | baseline | within +5% |

Gap-vs-SQLite after Attack 6 (both engines in NORMAL):

| Scenario | Expected gap |
|----------|-------------:|
| insert_autocommit | ≤ 2× |
| insert_batch | ≤ 35× (engine bottleneck, not fsync) |
| point_lookup | unchanged at ~25× |

The point of Attack 6 is the autocommit scenario AND fair comparison —
NOT to close every gap.

## Dependencies

- Depends on:
  - `WalDurabilityPolicy::{Strict, Normal, Off}` (already present in
    `axiomdb_wal::txn::TxnManager`).
  - `SessionContext` (already present).
  - `SET name = value` parser (already present —
    [`exec_dispatch.rs:797`](crates/axiomdb-sql/src/executor/exec_dispatch.rs)).
- Blocks:
  - Future: `SELECT @@synchronous` introspection.
  - Future: `axiomdb.toml` default override.
  - Future: full `PRAGMA synchronous = ...` parser
    (SQLite-syntax alias for `SET`).

## Open questions

All resolved during brainstorm; nothing pending.

## Done criteria

- [ ] `SET synchronous = 'NORMAL' | 'STRICT' | 'OFF' | DEFAULT` parses
      and updates `SessionContext.synchronous`.
- [ ] Default `SessionContext.synchronous == SessionDurability::Strict`
      (no regression in durability for users who don't opt in).
- [ ] Numeric and SQLite-alias forms accepted (`0`, `1`, `2`, `3`, `4`,
      `'OFF'`, `'NORMAL'`, `'FULL'`, `'EXTRA'`).
- [ ] `SET synchronous` issued inside `BEGIN..COMMIT` returns
      `DbError::InvalidValue` and DOES NOT change the value.
- [ ] `axiomdb_bench --compare --rows 10000` (with bench setting AxiomDB
      to NORMAL too) shows `insert_autocommit ≥ 60K rows/s`.
- [ ] `cargo nextest run --workspace` (Lima) — clean.
- [ ] `cargo clippy --workspace -- -D warnings` (Lima) — clean.
- [ ] `cargo fmt --check` — clean.
- [ ] `tools/wire-test.py` — clean (pre-flight per memory).
- [ ] 11 new integration tests covering every edge-case bullet.
- [ ] Rustdoc on `SessionDurability`, `parse_synchronous_setting`,
      `SessionContext::synchronous` / `set_synchronous`.
- [ ] Doc note in `docs/perf-sqlite-gap.md` documenting the
      durability tradeoff and the bench-fairness fix.

## References

External:
- SQLite `PRAGMA synchronous` semantics (WAL mode):
  [`research/sqlite/src/pager.c:3590-3611`](research/sqlite/src/pager.c)
- SQLite PRAGMA handler (the inside-txn check + value parsing):
  [`research/sqlite/src/pragma.c:1132-1148`](research/sqlite/src/pragma.c)
- SQLite `getSafetyLevel` (value parsing, 0..4 + alias names):
  [`research/sqlite/src/pragma.c:540-580`](research/sqlite/src/pragma.c)

Internal:
- Existing `WalDurabilityPolicy`:
  [`crates/axiomdb-wal/src/txn.rs:273`](crates/axiomdb-wal/src/txn.rs)
- Default = Strict:
  [`crates/axiomdb-wal/src/txn_construction.rs:17,39`](crates/axiomdb-wal/src/txn_construction.rs)
- Commit branching by policy:
  [`crates/axiomdb-wal/src/txn_begin_commit.rs:111-127`](crates/axiomdb-wal/src/txn_begin_commit.rs)
- Existing `SET` dispatcher:
  [`crates/axiomdb-sql/src/executor/exec_dispatch.rs:797`](crates/axiomdb-sql/src/executor/exec_dispatch.rs)
- Bench sets SQLite to NORMAL:
  [`benches/comparison/axiomdb_bench/src/main.rs:464`](benches/comparison/axiomdb_bench/src/main.rs)
- Brainstorm: this conversation, 2026-05-17.
