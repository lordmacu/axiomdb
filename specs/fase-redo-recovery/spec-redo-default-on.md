# Spec: redo-default-on — make `RedoMode::FrameOnly` the embedded default (autocommit win + close the data-loss hole)

Phase: redo-recovery (project B) — final rollout subphase
Task: Lever B — flip the embedded `Db` default to frame-only redo, gated by the
full crash suite (T1–T7) and a no-regression A/B.
Status: draft

## Context

Frame-only redo (`RedoMode::FrameOnly`) is fully built and shipped as an opt-in
(6f closed for embedded): writes go to the page-frame log, a commit is durable
via the frame fsync, the per-commit main-file `sync_all` is dropped, and a
background `FrameCheckpointer` bounds the log. It is **off by default**
(`DbConfig::redo = None → RedoMode::Off`) because the comprehensive crash suite
(T1–T7, the safety gate) was deferred.

Two reasons to make it the default now, measured this session:

1. **Correctness (the real driver):** with redo off, recovery is UNDO-only and
   nothing flushes the main file per commit, so a committed write lost before
   the next checkpoint is **gone after a crash** — a live data-loss hole that
   `t0_committed_heap_insert_survives_power_loss` proves (green only with redo
   on). Default-on closes it for every embedded user.
2. **Performance:** clean interleaved A/B (`--scenario insert_autocommit`,
   OFF/ON ×4) shows **autocommit ~1.56×** (OFF ~22.6K vs ON ~35.3K ops/s,
   ON > OFF every run), taking the autocommit gap vs SQLite ~3.1× → ~2.0×.
   Batch is a wash (frame-append cancels the dropped sync_all — measured prior);
   reads are expected neutral (to be re-confirmed).

## Goal

Make `RedoMode::FrameOnly` the default for the **embedded `Db`**, with the full
crash suite (T1–T7) green on the fault-injection harness and a clean
no-regression A/B, so the data-loss hole is closed and autocommit is ~1.5×
faster out of the box.

## Non-goals

- **Server default-on** — deferred. `axiomdb-network::SharedDatabase` does not
  yet wire the `FrameCheckpointer` (6f step 6), so the server would grow the
  frame log unbounded under default-on. The server stays opt-in until 6f step 6;
  this spec scopes the flip to embedded only.
- Changing the redo mechanism — it is built (subphases 3–6f); this is the
  validation + rollout, not new recovery code (bug fixes excepted if T1–T7 find
  them).
- Batch perf — frame-only is a wash for batch; not a goal here.
- Removing `RedoMode::Off` — it stays available (opt-out / dev / the A/B knob).

## Behavior

### Config change

```rust
// DbConfig::default(): redo: None  →  redo: Some(RedoMode::FrameOnly)
// (or resolved_redo() fallback None → FrameOnly). resolved_redo() then
// returns FrameOnly unless a config/caller explicitly sets redo = "off".
```

- An explicit `redo = "off"` in TOML or `DbConfig { redo: Some(Off), .. }` must
  still resolve to `Off` (opt-out preserved).
- The embedded open path already spawns the checkpointer + installs the wake
  hook when the resolved mode is `FrameOnly` (6f step 5) — so flipping the
  default automatically activates the bounded-log machinery. Verify the open
  path keys off `resolved_redo()`, not `config.redo.is_some()`.

### Crash suite (T1–T7) — the safety gate

Built on the fault-injection harness (`FaultInjectionStorage`: power loss =
durable→current revert; the frame log survives). T0 is done + green.

| # | Scenario | Asserts |
|---|----------|---------|
| T1 | redo of each page type | a committed write to heap, clustered leaf, clustered internal, secondary index, overflow, and free-list/meta pages each survives power loss + recovery |
| T2 | idempotent replay | crash *during* recovery → a second recovery yields the identical state (pageLSN strict-`>` guard holds) |
| T3 | partial checkpoint | crash mid-checkpoint → redo from the previous checkpoint LSN restores all committed data; no double-apply |
| T4 | torn page | a single half-written page is repaired by frame replay (+ doublewrite); checksum mismatch never surfaces post-recovery |
| T5 | UNLOGGED tables | still truncated on dirty open; logged tables + a clean reopen preserve their rows (the `integration_open_integrity` guarantee holds under frame-only) |
| T6 | undo no-regression | an uncommitted txn at crash is still fully UNDONE (logical undo + redo coexist) |
| T7 | soak (bounded) | randomized op streams (insert/update/delete across heap + clustered + secondary) with random crash points, replayed vs an in-memory oracle; bounded iterations (e.g. 200 seeds) so it runs in CI |

### No-regression A/B (clean, interleaved, same conditions)

- autocommit: ON ≥ OFF (the win) — already ✓ (~1.56×).
- batch: ON ≈ OFF within noise (wash) — re-confirm interleaved.
- reads (`full_scan`, `select_where`, `point_lookup`, `range_scan`): ON ≈ OFF
  (WAL-aware read adds a wal-index probe; memory says neutral) — confirm
  interleaved, no scenario regresses > noise.

## Edge cases

- [ ] Explicit `redo = "off"` opt-out still resolves Off (config + struct).
- [ ] Empty DB first-open under default frame-only (bootstrap writes frames).
- [ ] Reopen a DB created under `Off` with the new default `FrameOnly` (and vice
      versa) — mode is per-open, not on-disk; both must open + recover correctly.
- [ ] Checkpointer lifecycle: `Drop for Db` joins + final checkpoint (6f) — no
      lost frames on clean shutdown under the new default.
- [ ] Hard-cap back-pressure: a burst that outruns the checkpointer checkpoints
      inline at `max_wal_size_mb × hard_multiplier` (log stays bounded).
- [ ] Crash with the frame log mid-`recycle` (post-checkpoint) → recovery redoes
      leftovers idempotently (T2/T3 overlap).
- [ ] Torn frame header (not just torn page) → scan stops at salt mismatch, no
      false redo (T4 overlap).

## Performance budget

| Scenario | Target |
|----------|--------|
| insert_autocommit (300) | ON ≥ 1.4× OFF (measured ~1.56×) |
| insert_batch (50K) | ON within ±5% of OFF (wash, no regression) |
| reads (full_scan/point_lookup/select_where/range_scan) | ON within ±5% of OFF |

## Dependencies

- Depends on: subphases 3–6f (built); `FaultInjectionStorage` harness; the
  embedded checkpointer wiring (6f step 5).
- Blocks: server default-on (needs 6f step 6 first — out of scope here).

## Open questions

- **Flip via `Default::default()` or `resolved_redo()` fallback?** → Lean
  `resolved_redo()` fallback (`None → FrameOnly`): keeps `DbConfig::default()`
  honest about "unset" and centralizes the policy. Decide in plan.
- **T7 soak size / where it runs?** → bounded (≤ ~200 seeds) so it's a normal
  `nextest` test, not a separate long-running job. Confirm in plan.
- **MmapStorage in-process power loss?** → can't (mmap MAP_SHARED survives
  SIGKILL); T1–T7 use `FaultInjectionStorage` for the power-loss model + a real
  multi-session reopen for the MmapStorage apply branch (per subphase 5).

## Done criteria

- [ ] `resolved_redo()` defaults to `FrameOnly`; explicit `Off` opt-out works;
      unit tests for both.
- [ ] Embedded open path activates the checkpointer off `resolved_redo()`.
- [ ] T1–T7 implemented + green on the fault-injection harness.
- [ ] No-regression A/B: autocommit ON ≥ OFF; batch + reads within ±5%
      (interleaved, documented numbers).
- [ ] `cargo nextest run --workspace` green (Lima); clippy + fmt clean.
- [ ] Server still opt-in (its default unchanged / explicit Off until 6f step 6)
      — verified no unbounded-log regression on the server path.
- [ ] docs: `user-guide/features/transactions.md` + `internals/wal.md` (default
      is now frame-only redo; opt-out documented); memory updated.

## References

- `specs/fase-redo-recovery/spec-redo-recovery.md` — master spec, T0–T7 + DoD.
- `crates/axiomdb-storage/src/config.rs` — `RedoMode`, `resolved_redo()`.
- `crates/axiomdb-storage/src/fault_injection.rs` — power-loss harness.
- `crates/axiomdb-wal/tests/integration_redo_recovery.rs` — T0.
- `crates/axiomdb-network/tests/integration_open_integrity.rs` — the
  logged-survives / unlogged-truncates guarantee (T5).
- `research/sqlite/src/wal.c` — `walCheckpoint`, recovery model.
