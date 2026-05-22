# Plan: Frame-only redo — auto-checkpoint trigger + opt-in exposure

Phase: redo-recovery (project B) — subphase 6f (Lever 2 / Task 1)
Spec: specs/fase-redo-recovery/spec-subfase-6f-frame-checkpoint-trigger.md
Status: in-progress
Effort: **max** (durability/commit path + a background thread)

## Progress (2026-05-22)

- [x] **Step 1** — `TxnManager::is_committed` predicate (`fe082e0e`)
- [x] **Step 2** — `maybe_checkpoint_frames` + `frame_log_durable_len` (`491687b8`)
- [x] **Step 3** — synchronous back-pressure in `commit_durable` (hard cap) (`a3680dc8`)
- [ ] **Step 4** — `FrameCheckpointer` background thread. NOTE: the existing
      `axiomdb_wal::Checkpointer` (checkpoint.rs) is the STATELESS *logical-WAL*
      checkpoint (flush pages + record Checkpoint LSN) — unrelated; the frame
      checkpointer is new (name `FrameCheckpointer`, no collision). Needs Arc-shared
      storage+txn (Step 5's refactor) so the thread holds handles.
- [ ] **Step 5** — embedded `Db` wiring (Arc-share + Drop join + set the hard cap
      from `DbConfig.max_wal_size_mb × K`). **Until the cap is wired here/Step 7 the
      opt-in is NOT yet bounded** (hard defaults to `u64::MAX`); the *mechanism*
      (steps 1-3) is complete + tested.
- [ ] **Step 6** — server `SharedDatabase` wiring. **Step 7** — config K + watchdog.
      **Step 8** — docs + A/B + close.

## Summary

Wire a production trigger for the existing `checkpoint_frames` mechanism so
frame-only redo bounds its frame log. Build bottom-up: (1) a committed-txn
predicate on `TxnManager`; (2) a guarded `maybe_checkpoint_frames` + a
`frame_log_durable_len` accessor on storage; (3) synchronous back-pressure at the
commit boundary (the robustness net — log never unbounded even with no thread);
(4) the background `FrameCheckpointer` (soft-threshold, Condvar-driven); (5) wire
into embedded `Db` (Arc-share storage+txn, spawn on redo, join+final-checkpoint on
Drop); (6) wire into server `SharedDatabase` (spawn on redo, join on shutdown);
(7) config (soft=`max_wal_size_mb`, hard=`K×soft`) + thread-death watchdog;
(8) docs + A/B re-confirm + close. Order = bottom-up so each step compiles + tests
in its own crate before the cross-crate wiring (steps 5–6).

## Dependencies

Must be done first:
- [x] spec-subfase-6f approved
- [x] checkpoint_frames mechanism (6b), frame-only write/read + read_page_if_for, in-flight recycle — DONE

Blocks:
- [ ] Lever 2 / Task 2 (crash suite T1–T7 — crash-tests this trigger)
- [ ] redo default-on

## Affected files

New:
- `crates/axiomdb-storage/src/checkpointer.rs` — `FrameCheckpointer` (bg thread + Condvar + stop + join)
- `crates/axiomdb-storage/tests/integration_checkpoint_trigger.rs` — bound/back-pressure/thread-death tests

Modified:
- `crates/axiomdb-wal/src/*` — `TxnManager::is_committed(txn_id)` accessor (+ unit test)
- `crates/axiomdb-storage/src/mmap.rs` — `maybe_checkpoint_frames(is_committed, force)`, `frame_log_durable_len()`
- `crates/axiomdb-storage/src/engine.rs` — trait methods (default no-op for non-mmap)
- `crates/axiomdb-storage/src/config.rs` — `checkpoint_hard_multiplier` (K) default
- `crates/axiomdb-wal/src/txn_begin_commit.rs` (or commit boundary) — back-pressure hook
- `crates/axiomdb-embedded/src/lib.rs` — Arc-share storage+txn, spawn checkpointer, `Drop`
- `crates/axiomdb-network/src/mysql/shared_db.rs` — spawn checkpointer, shutdown join
- `docs-site/src/user-guide/features/transactions.md`, `docs-site/src/internals/wal.md`

---

## Step 1 — `TxnManager::is_committed` predicate

**Goal:** a cheap, correct "is this txn committed + durable" check for the checkpoint predicate.
**Files:** `crates/axiomdb-wal/src/txn_begin_commit.rs` (or where `max_committed`/active live) + unit test.
**Approach:** TDD. Investigate the exact semantics first — a frame is safe to checkpoint only if its txn's Commit is durable. Candidate: `txn_id <= max_committed.load(Acquire) && !active.contains(txn_id)`. Confirm against group-commit (`max_committed` advances only after the fsync pipeline drives it — so `<= max_committed` ⇒ durable).

### Test
```rust
// committed txn → true; active/in-flight → false; future id → false;
// after group-commit advance, the committed id flips true.
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-wal is_committed
```

### Commit
`feat(redo-recovery): 6f step 1 — TxnManager::is_committed predicate`

---

## Step 2 — storage `maybe_checkpoint_frames` + `frame_log_durable_len`

**Goal:** a guarded checkpoint entry (the single chokepoint used by both the bg thread and back-pressure) + a size accessor.
**Files:** `mmap.rs`, `engine.rs` (trait + default no-op), `tests/integration_checkpoint_trigger.rs`.
**Approach:** TDD. `maybe_checkpoint_frames(is_committed, force)`: if `!frame_log_active()` → 0; if `!force && durable_len < soft` → 0; else acquire the checkpoint write-lock + run the existing `checkpoint_frames(is_committed)`. `frame_log_durable_len()` reads the frame log's durable offset.

### Test
```rust
// under soft → returns 0, log unchanged;
// over soft → checkpoints, log recycled (durable_len drops);
// force=true under soft → still checkpoints;
// redo off → 0 (no-op).
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage integration_checkpoint_trigger
./tools/vm.sh clippy
```

### Commit
`feat(redo-recovery): 6f step 2 — maybe_checkpoint_frames + frame_log_durable_len`

---

## Step 3 — synchronous back-pressure at the commit boundary

**Goal:** guarantee the log is bounded even with NO background thread (the robustness net).
**Files:** the commit boundary (`commit_durable` / `txn_begin_commit.rs`) — it already has `storage` + `self` (txn).
**Approach:** TDD. After a frame-only commit, if `storage.frame_log_durable_len() > hard` (= K×soft), call `storage.maybe_checkpoint_frames(|t| self.is_committed(t), /*force=*/true)`. Only on the frame-only path; redo off unchanged.

### Test
```rust
// frame-only, NO background thread: insert >> hard in autocommit;
// assert the log never exceeds ~hard (back-pressure fired inline).
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage back_pressure   # or wal, wherever the boundary lands
```

### Commit
`feat(redo-recovery): 6f step 3 — synchronous checkpoint back-pressure (hard cap)`

---

## Step 4 — `FrameCheckpointer` background thread

**Goal:** off-commit-path checkpoints at the soft threshold (preserve the ~2× win, no commit latency spikes).
**Files:** `crates/axiomdb-storage/src/checkpointer.rs` + tests.
**Approach:** TDD. `FrameCheckpointer { handle: JoinHandle, stop: Arc<AtomicBool>, cv: Arc<Condvar>, ... }`. Spawned with an `Arc<MmapStorage>` + a committed-predicate source (a `Fn() -> (impl Fn(u64)->bool)` or an `Arc<TxnManager>`). Loop: wait on Condvar (with a timeout fallback) until `stop` or `durable_len >= soft`; on wake, `maybe_checkpoint_frames(is_committed, false)`. `notify()` called by frame append when it crosses soft (cheap atomic check + `cv.notify_one()`). `stop_and_join()` signals + final checkpoint + joins.

### Test
```rust
// threaded: sustained appends → bg checkpoints fire → durable_len stays ≤ ~soft;
// stop_and_join → final checkpoint runs, thread joins, no leak;
// thread "killed" (drop handle without join path) → step-3 back-pressure still bounds.
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage checkpointer
```

### Commit
`feat(redo-recovery): 6f step 4 — FrameCheckpointer background thread`

---

## Step 5 — wire into embedded `Db`

**Goal:** embedded frame-only opt-in spawns the checkpointer + joins on Drop.
**Files:** `crates/axiomdb-embedded/src/lib.rs`.
**Approach:** TDD. Arc-share so the thread can hold handles: `Db.storage: Arc<MmapStorage>`, `Db.txn: Arc<TxnManager>` (call sites use `&*self.storage` / `&*self.txn`; methods are `&self` so Deref keeps most working). On `open_with_config` with `redo = FrameOnly`, spawn `FrameCheckpointer` (clones of the Arcs); store the handle. Add `impl Drop for Db` → `checkpointer.stop_and_join()` (final checkpoint). Redo off → no thread, no Arc cost difference observable.

### Test
```rust
// embedded frame-only: insert >> soth threshold → log bounded;
// drop(db) → final checkpoint ran (reopen: empty/bounded log, rows present);
// redo off → no checkpointer, behavior unchanged.
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-embedded
cargo build --release -p axiomdb-bench-comparison && \
  AXIOMDB_BENCH_REDO=1 ./target/release/axiomdb_bench --scenario insert_autocommit --rows 5000   # ~2x preserved
```

### Commit
`feat(redo-recovery): 6f step 5 — embedded Db spawns + joins the checkpointer`

---

## Step 6 — wire into server `SharedDatabase`

**Goal:** server frame-only opt-in spawns the checkpointer + joins on shutdown.
**Files:** `crates/axiomdb-network/src/mysql/shared_db.rs` (+ server shutdown path).
**Approach:** TDD. `SharedDatabase` is shared via `Arc<SharedDatabase>`; spawn the checkpointer in `open_with_config` when redo on (holding an `Arc` to storage/txn or `Weak<SharedDatabase>`); join on the server shutdown signal. Mirror the embedded back-pressure (Step 3 already covers the commit boundary, shared).

### Test
```rust
// network frame-only: sustained writes → log bounded; clean shutdown → joined + final checkpoint.
```

### Verification
```bash
./tools/vm.sh test -p axiomdb-network integration_open_integrity
```

### Commit
`feat(redo-recovery): 6f step 6 — server SharedDatabase checkpointer + shutdown join`

---

## Step 7 — config (soft/hard) + thread-death watchdog

**Goal:** `hard = K × soft` (K default, e.g. 2), soft = `max_wal_size_mb`; surface a checkpointer-thread death.
**Files:** `config.rs` (+ a `checkpoint_hard_multiplier` default), checkpointer (watchdog log line).
**Approach:** TDD. Config plumbs K; the checkpointer logs `error!` if it exits unexpectedly; back-pressure (Step 3) is the functional safety net.

### Verification
```bash
./tools/vm.sh test -p axiomdb-storage config
```

### Commit
`feat(redo-recovery): 6f step 7 — checkpoint thresholds config + watchdog`

---

## Step 8 — docs + A/B re-confirm + close

**Goal:** document the opt-in + verify the spec's done criteria end-to-end.
**Files:** `docs-site/src/user-guide/features/transactions.md` (the `redo = "frame_only"` opt-in + semantics + "opt-in until the crash suite"), `docs-site/src/internals/wal.md` (checkpoint trigger), `memory/project_insert_perf.md` (lever-2 update).

### Verification against spec done criteria
- [ ] frame-only sustained writes → log bounded (Step 5/6 tests)
- [ ] back-pressure at hard cap (Step 3 test)
- [ ] thread join clean + final checkpoint; thread-death degrades to sync (Step 4/5)
- [ ] autocommit ~2× + no read regression re-confirmed: `--compare` redo on vs off
- [ ] `cargo nextest --workspace` + clippy + fmt clean (Lima)
- [ ] redo off default unchanged

### Final commit
`feat(redo-recovery): 6f — frame-only redo opt-in (auto-checkpoint trigger)`

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `is_committed` semantics wrong under group commit → checkpoints an uncommitted frame (data loss) | medium | Step 1 investigate + test; conservative `<= max_committed && !active`; recovery's pageLSN guard is a second line |
| Embedded Arc-refactor ripples through call sites | medium | mechanical (Deref); isolate in Step 5; `&*self.storage` at the `&dyn` sites |
| bg thread + commit back-pressure concurrency (double checkpoint) | low | both go through `maybe_checkpoint_frames` under the single checkpoint write-lock |
| checkpointer thread death → silent unbounded log | low | Step 3 back-pressure bounds regardless; Step 7 watchdog log |
| latency spike when back-pressure fires | low | tune K so it rarely fires; bg thread handles steady state |

## Rollback plan

Each step is an isolated commit. To abandon: `git reset --hard <commit before step N>`; redo stays opt-in (default off), so no production impact. The frame-only mechanism + flag remain (already landed pre-6f).

## Estimated effort

Total: ~3–5 days (max effort). Steps 1–4 (engine, ~1 crate each): ~0.5 day each. Step 5 (embedded Arc-refactor): ~1 day. Step 6 (server): ~0.5 day. Steps 7–8: ~0.5 day.
