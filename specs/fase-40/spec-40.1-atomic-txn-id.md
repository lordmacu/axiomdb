# Spec: 40.1 — Atomic Transaction ID & Snapshot

## What to build (not how)

Make `next_txn_id` and `max_committed` in `TxnManager` atomic so that future
concurrent writers can safely allocate transaction IDs and read snapshots without
data races. Enable `snapshot()` and `active_snapshot()` to work with `&self`
instead of `&mut self` (lock-free snapshot creation).

This subfase does NOT enable concurrent writers — the single-writer
`Arc<RwLock<Database>>` stays intact. It only prepares the atomic primitives.

## Inputs / Outputs

- Input: Current `TxnManager` with `next_txn_id: u64` and `max_committed: u64`
- Output: `TxnManager` with `next_txn_id: AtomicU64` and `max_committed: AtomicU64`
- Errors: None new — all existing error paths unchanged

## Research findings

| Database | TxnId Type | Allocation | Ordering |
|---|---|---|---|
| **InnoDB** | 64-bit `Atomic_counter` | `fetch_add(1, relaxed)` | Release/Acquire for visibility |
| **PostgreSQL** | 32-bit + epoch | Lock-protected (`XidGenLock`) | Heavyweight locks as barriers |
| **SQLite** | WAL frame numbers | Sequential (single writer) | Memory-mapped shared state |
| **DuckDB** | 64-bit locked | `mutex` protected | Mutex as barrier |

**Chosen approach:** InnoDB pattern — `AtomicU64` with `Relaxed` for ID allocation,
`Release` on commit, `Acquire` on snapshot. Simpler than PostgreSQL's lock-based
approach, proven at scale by InnoDB.

## Fields to change

### `next_txn_id: u64` → `AtomicU64`

| Location | Current | New |
|---|---|---|
| Field declaration (line 127) | `next_txn_id: u64` | `next_txn_id: AtomicU64` |
| create() (line 171) | `next_txn_id: 1` | `AtomicU64::new(1)` |
| open() (line 192) | `next_txn_id: max_committed + 1` | `AtomicU64::new(max_committed + 1)` |
| begin_with_isolation() (lines 231-232) | `let txn_id = self.next_txn_id; self.next_txn_id += 1;` | `let txn_id = self.next_txn_id.fetch_add(1, Relaxed);` |
| open_with_recovery() (line 1339) | `next_txn_id: result.max_committed + 1` | `AtomicU64::new(result.max_committed + 1)` |

### `max_committed: u64` → `AtomicU64`

| Location | Current | New |
|---|---|---|
| Field declaration (line 128) | `max_committed: u64` | `max_committed: AtomicU64` |
| create() (line 172) | `max_committed: 0` | `AtomicU64::new(0)` |
| open() (line 193) | `max_committed: <scan_result>` | `AtomicU64::new(<scan_result>)` |
| begin_with_isolation() (line 256) | `self.max_committed + 1` | `self.max_committed.load(Acquire) + 1` |
| commit() — 4 locations (lines 293, 305, 312, 318) | `self.max_committed = txn_id` | `self.max_committed.store(txn_id, Release)` |
| advance_committed() (lines 378-379) | `if max > self.max_committed { self.max_committed = max; }` | `self.max_committed.fetch_max(max, Release)` |
| advance_committed_single() (lines 388-389) | `if txn_id > self.max_committed { self.max_committed = txn_id; }` | `self.max_committed.fetch_max(txn_id, Release)` |
| snapshot() (line 1234) | `self.max_committed` | `self.max_committed.load(Acquire)` |
| active_snapshot() (line 1252) | `self.max_committed + 1` | `self.max_committed.load(Acquire) + 1` |
| max_committed() accessor (line 1264) | `self.max_committed` | `self.max_committed.load(Acquire)` |
| open_with_recovery() (line 1340) | `max_committed: result.max_committed` | `AtomicU64::new(result.max_committed)` |

### Method signatures that change

| Method | Current | New | Why |
|---|---|---|---|
| `snapshot()` | `&mut self` or `&self` | `&self` (guaranteed) | AtomicU64::load only needs shared ref |
| `active_snapshot()` | depends on `&mut self` | `&self` possible | Same reason |
| `max_committed()` | accessor | `&self` | Same reason |

## Memory ordering rationale

```
Relaxed:  next_txn_id.fetch_add — uniqueness only needs atomicity, not ordering.
          Two threads getting IDs 5 and 6 don't care which "happened first" in
          terms of other memory operations.

Release:  max_committed.store in commit() — ensures all WAL writes and page
          mutations are visible to other threads BEFORE the new max_committed
          value becomes visible. Without this, a reader could see the new
          max_committed but stale page data.

Acquire:  max_committed.load in snapshot() — pairs with Release in commit().
          Ensures the reader sees all mutations that happened before the commit
          that set the max_committed value it reads.
```

## Use cases

1. **Happy path (single writer, today):** No behavior change. Atomics are ~1ns
   overhead per operation. All tests pass unchanged.

2. **Future: two threads call begin() simultaneously (40.2+):**
   `fetch_add(1, Relaxed)` guarantees unique IDs. Thread A gets 5, thread B gets 6
   (or vice versa). No duplicates.

3. **Future: writer commits while reader takes snapshot (40.2+):**
   Writer stores `max_committed = 10` with Release. Reader loads with Acquire and
   sees either 9 (old) or 10 (new) — never a torn value, never sees 10 without
   the commit's mutations being visible.

4. **Future: pipeline advance_committed with concurrent commits (40.4+):**
   `fetch_max` atomically advances to the highest txn_id without check-then-write race.

## Acceptance criteria

- [ ] `next_txn_id` is `AtomicU64` — `fetch_add(1, Relaxed)` in begin()
- [ ] `max_committed` is `AtomicU64` — `store(txn_id, Release)` in commit()
- [ ] `snapshot()` reads `max_committed` with `load(Acquire)` — works with `&self`
- [ ] `advance_committed()` uses `fetch_max(max, Release)` — no check-then-write race
- [ ] All 5 initialization paths updated (create, open, open_with_recovery, 2×)
- [ ] All existing WAL + SQL tests pass unchanged
- [ ] `cargo clippy -- -D warnings` clean
- [ ] No performance regression (atomics are negligible)

## Out of scope

- Moving `active: Option<ActiveTxn>` to per-connection (that's 40.2)
- Changing `Arc<RwLock<Database>>` (that's 40.10)
- Changing `StorageEngine` trait signature (that's 40.3)
- Any actual concurrent writer behavior (single-writer still enforced)

## Dependencies

- None — this is the first subfase, no dependencies on other 40.x changes
- Phase 1-7 (WAL, transactions) ✅
