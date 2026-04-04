# Plan: 40.1 — Atomic Transaction ID & Snapshot

## Files to modify

| File | Change |
|---|---|
| `crates/axiomdb-wal/src/txn.rs` | `next_txn_id` and `max_committed` → AtomicU64; update all 14 access points |
| `crates/axiomdb-wal/src/lib.rs` | Update `pub use` if method signatures change |

## Algorithm / Data structure

No new algorithms. Pure mechanical replacement:

```rust
// BEFORE
struct TxnManager {
    next_txn_id: u64,
    max_committed: u64,
    ...
}

// AFTER
use std::sync::atomic::{AtomicU64, Ordering};

struct TxnManager {
    next_txn_id: AtomicU64,
    max_committed: AtomicU64,
    ...
}
```

Key patterns:
```rust
// ID allocation (begin)
let txn_id = self.next_txn_id.fetch_add(1, Ordering::Relaxed);

// Commit visibility
self.max_committed.store(txn_id, Ordering::Release);

// Snapshot read
let snap_id = self.max_committed.load(Ordering::Acquire) + 1;

// Advance (pipeline)
self.max_committed.fetch_max(new_max, Ordering::Release);
```

## Implementation phases

1. Add `use std::sync::atomic::{AtomicU64, Ordering};` import
2. Change field types in TxnManager struct
3. Update all 5 initialization paths (create, open, open_with_recovery)
4. Update begin_with_isolation() — fetch_add for next_txn_id, load for snapshot
5. Update commit() — 4 store locations for max_committed
6. Update advance_committed / advance_committed_single — fetch_max
7. Update snapshot() / active_snapshot() / max_committed() — load with Acquire
8. Verify snapshot methods work with &self (not &mut self)
9. Run all tests

## Tests to write

No new tests needed — this is a transparent change. All existing tests validate
the same behavior. The atomics are invisible to the test harness because
single-threaded execution serializes all access anyway.

Optional: Add a compile-time assertion that TxnManager is Send (it should be
with AtomicU64 fields).

## Anti-patterns to avoid

- DO NOT use `Ordering::SeqCst` everywhere — it's a performance tax with no
  benefit here. Use the minimum ordering that provides correctness.
- DO NOT change `active: Option<ActiveTxn>` — that's 40.2's scope.
- DO NOT try to remove the `Arc<RwLock<Database>>` — that's 40.10's scope.
- DO NOT add any `Mutex` — atomics are sufficient for these two counters.

## Risks

- **fetch_max** requires Rust 1.45+ (AtomicU64::fetch_max is stable since 1.45).
  AxiomDB uses recent Rust, so this is fine.
- **Ordering correctness**: Release/Acquire pairing must be consistent. If any
  access path uses wrong ordering, MVCC visibility could break under concurrency.
  Mitigation: the single-writer constraint means this is invisible until 40.2+,
  giving us time to validate.
