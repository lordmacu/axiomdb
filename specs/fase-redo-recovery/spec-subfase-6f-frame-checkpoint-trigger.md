# Spec: Frame-only redo — auto-checkpoint trigger + opt-in exposure

Phase: redo-recovery (project B) — subphase 6f (Lever 2 / Task 1: flag-gated frame-only redo)
Status: approved
Effort: **max** (commit/durability path + a background thread; a wrong checkpoint can lose committed data)

> **Trigger design (resolved 2026-05-22, user delegated "most optimal/robust/scalable"):**
> a **background checkpointer thread + synchronous back-pressure** (Postgres checkpointer /
> InnoDB page-cleaner model), owned at the Db/Database layer. See "Behavior → Trigger".

## Context

`RedoMode::FrameOnly` (the SQLite-WAL write-ahead path) is already implemented
end-to-end on this branch: write-ahead frame log, wal-index reads with page_id+salt
verification (`read_page_if_for`, mmap.rs:786 + test), in-flight-safe recycle
(mmap.rs:1030 + tests), REDO recovery (T0 green), and the per-commit main-file
`sync_all` is dropped when `frame_log_active()` (bootstrap.rs:454). The flag is
exposed: embedded `Db::open_with_config(DbConfig{redo})` and server
`axiomdb.toml` `redo = "frame_only"` (RedoMode is `Deserialize`, snake_case).

**Measured this session (macOS, fase-redo-recovery):** autocommit 19.5K→38.2K ops/s
(**~2×**, gap vs SQLite 6×→3×) by dropping the per-commit `sync_all`; reads NO
regression (point_lookup 223K→401K, full_scan 9.2M→10M); batch is a wash
(frame-append +18ms ≈ amortized sync_all — Task 3).

**The one missing piece for a safe opt-in:** `StorageEngine::checkpoint_frames`
(the frames→main apply + log recycle, subphase 6b) has **no production trigger** —
it is only ever called from tests (verified: all call sites are in
`mmap.rs`/`fault_injection.rs` test modules). So in frame-only mode the frame log
**grows unbounded → disk exhaustion**. This subphase wires the trigger.

## Goal

Make frame-only redo a **shippable opt-in**: bound the frame log with an automatic
checkpoint trigger, and document the opt-in + its durability semantics.

## Non-goals

- **Crash suite T1–T7** — Lever 2 / Task 2; the gate to flip redo *default-on*.
  Opt-in users accept the not-yet-fully-crash-proven status.
- **Frame-log file reuse / batch-write win** — Lever 2 / Task 3 (the +18ms frame-append).
- **Making redo the default** — stays `Off`. This is opt-in only.
- Re-doing already-done correctness: read frame-header verification, in-flight-safe
  recycle, the frame-only write/read mechanism, flag plumbing. **Verify, don't rebuild.**

## Behavior

### Trigger semantics (background checkpointer + synchronous back-pressure)

Modeled on Postgres's checkpointer / InnoDB's page cleaner: a dedicated background
thread does the checkpoints off the commit path (so commits never pay the apply+fsync
latency → the autocommit ~2× win is preserved without spikes), and a synchronous
back-pressure path guarantees the log is ALWAYS bounded even if the thread falls
behind or dies. Owned at the **Db/Database layer** (where both the storage engine and
the `TxnManager` committed-set are accessible), NOT per-connection.

- **Soft threshold → background checkpoint.** When the frame log's durable size crosses
  `soft = DbConfig.max_wal_size_mb`, signal the checkpointer (a `Condvar`). It acquires
  the checkpoint write-lock (appends take read) and runs
  `checkpoint_frames(committed_set)` → apply committed frames → fsync main → recycle.
  Commits keep going concurrently (frame appends take the read side).
- **Hard cap → synchronous back-pressure.** If the durable size crosses
  `hard = K × soft` (the checkpointer is behind or dead), the *committing* path runs the
  checkpoint inline before returning — bounded latency spike, but the log is never
  unbounded. This is the robustness net.
- **Clean shutdown.** Signal the checkpointer to drain + run a final checkpoint, then
  join the thread, so a restart begins with a bounded/empty log + current main file.
- **Committed-set:** predicate from `TxnManager` (committed txn_ids); `build_index(is_committed)`
  already consumes it. Uncommitted in-flight frames are preserved by the recycle (done).
- **Thread death is safe:** if the checkpointer panics/exits, back-pressure still bounds
  the log (degraded to synchronous), and a watchdog log line surfaces it.

### Public API (sketch — exact shape decided in /plan-task)

```rust
// storage: guarded "checkpoint if over threshold" (no-op when redo Off / under soft).
// Used by BOTH the background thread (soft) and the commit back-pressure (hard).
fn maybe_checkpoint_frames(&self, is_committed: &dyn Fn(u64) -> bool, force: bool) -> Result<usize, DbError>;
fn frame_log_durable_len(&self) -> u64;            // for threshold checks
// Db/Database: spawn the checkpointer when redo is on; join + final checkpoint on close.
struct Checkpointer { /* JoinHandle + Condvar + stop flag */ }
```

### Invariants

- Precondition: `frame_log_active()`. Redo `Off` ⇒ every trigger is a no-op (today's behavior, byte-for-byte).
- Postcondition (after a checkpoint): all committed frames are durable in the main
  file; the frame log is recycled (bounded); uncommitted frames preserved.
- Invariant: under sustained writes the frame log stays ≤ ~`max_wal_size_mb`.

### Error cases

| Case | Behavior |
|------|----------|
| Concurrent appends during checkpoint | serialized by the existing `checkpoint_lock` (append=read, checkpoint=write) |
| Crash during checkpoint | recovery re-applies committed frames; pageLSN strict-`>` guard is idempotent (full matrix → Task 2) |
| Redo `Off` | trigger is a no-op |
| Empty frame log | checkpoint applies 0, no recycle churn |

## Edge cases

- [ ] Frame log crosses the threshold mid-large-txn → checkpoint fires only AFTER commit (cannot apply the active txn's uncommitted frames).
- [ ] Concurrent writers + checkpoint (multi-writer) → `checkpoint_lock` serializes (existing 8-writer racing test should extend to the triggered path).
- [ ] Clean shutdown with uncommitted in-flight txns → preserved or rolled back on close (define which).
- [ ] redo `Off` → no-op, no thread spawned (no behavior change, no perf cost).
- [ ] Empty / under-threshold log → no-op.
- [ ] Reopen after a triggered checkpoint → reads correct (main current + recycled log).
- [ ] **Back-pressure (hard cap):** background thread stalled/behind → a commit crossing `hard` checkpoints inline; the log never exceeds ~`hard`.
- [ ] **Checkpointer thread death:** thread panics/exits → back-pressure keeps the log bounded (degraded synchronous), surfaced by a log line — never silent unbounded growth.
- [ ] **Clean shutdown:** checkpointer drains + final checkpoint, thread joined (no leak, no lost bound).

## Performance budget

| Operation | Target |
|-----------|--------|
| Steady-state autocommit (frame-only) | preserve ~2× win (38K ops/s; checkpoint amortized every ~max_wal_size_mb, not per-commit) |
| Reads (frame-only) | no regression vs redo Off (walFindFrame + stale-offset fallback already handled) |
| Frame log size (sustained writes) | bounded ≤ ~max_wal_size_mb |

## Dependencies

- Depends on: `checkpoint_frames` (6b, done), `TxnManager` committed-set, the commit path.
- Blocks: Task 2 (crash suite crash-tests the trigger), redo default-on.

## Open questions

- [x] **Trigger location — RESOLVED:** background checkpointer thread + synchronous
  back-pressure (Postgres/InnoDB model), owned at the Db/Database layer. Chosen over
  synchronous-in-commit (latency spikes) and per-connection (decentralized/fragile) for
  optimal latency + scalability + robustness (see Trigger semantics).
- [ ] **(plan-time) Committed-set source:** confirm whether `TxnManager` already exposes
  the committed-txn set cheaply for the predicate or this subphase adds an accessor.
  Resolve during `/plan-task` (investigation, not a design fork).
- [ ] **(plan-time) Clean-shutdown + thread ownership:** exact hook — `Db::drop` (embedded,
  std::thread join) vs `Database` close / server shutdown signal (tokio task vs std::thread).
  Resolve during `/plan-task`.
- [ ] **(plan-time) Hard-cap multiplier `K`:** pick `hard = K × soft` (e.g. K=2) and the
  soft default; tune so back-pressure rarely fires in steady state.

## Done criteria

- [ ] Frame-only sustained writes keep the log bounded by the background checkpointer — a test inserts ≫ soft threshold and asserts the log recycled (size stays ≤ ~soft).
- [ ] Back-pressure test: with the background thread disabled/blocked, a commit crossing `hard` checkpoints inline; log ≤ ~`hard`.
- [ ] Checkpointer thread joins cleanly on close + a final checkpoint runs; thread death degrades to synchronous (not unbounded).
- [ ] Clean shutdown leaves a bounded/empty log + current main; reopen reads correctly.
- [ ] Opt-in documented: `docs-site` user durability page (`redo = "frame_only"` + semantics + "opt-in until the crash suite") + `internals/wal.md` (checkpoint trigger).
- [ ] autocommit ~2× + no read regression re-confirmed with the flag on (`--compare`); no regression on the redo-`Off` default path.
- [ ] `cargo nextest --workspace` + clippy + fmt clean (Lima).

## References

- `specs/fase-redo-recovery/spec-subfase-6b-checkpoint.md` (checkpoint_frames mechanism)
- `specs/fase-redo-recovery/spec-subfase-6c-frame-only.md` (the switch + read-race resolution)
- `memory/project_insert_perf.md` (the 6e + lever-2 measured sections)
- `crates/axiomdb-storage/src/{mmap.rs,config.rs}` (checkpoint_frames, read_page, recycle, RedoMode/DbConfig)
- `crates/axiomdb-catalog/src/bootstrap.rs:448-457` (the gated per-commit sync_all)
- `research/sqlite/src/wal.c` (sqlite3WalCheckpoint — PASSIVE/FULL/TRUNCATE, auto-checkpoint threshold)
