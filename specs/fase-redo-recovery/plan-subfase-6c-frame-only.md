# Plan: subphase 6c — the frame-only switch

Phase: redo-recovery (project B) — subphase 6c
Spec: specs/fase-redo-recovery/spec-subfase-6c-frame-only.md
Status: draft

## Summary

Flip writes to frame-only and drop the per-commit main-file flush, so a commit is durable
via the frame fsync alone (the autocommit ~2-3× win). Order: build the **storage-only**
pieces first (config mode, the safe lock-free read fallback, the checkpoint's wal_index
reconciliation + in-flight-safe recycle, frame-only `write_page` + enabling redo on open),
then the **axiomdb-catalog** change (drop/gate the per-commit `ensure_database_roots`
flush — the only cross-crate, coordination-sensitive step), then the trigger and the perf
A/B + close. Each storage step is independently testable; the switch only goes "live" when
the config mode is on, so steps 1–4 land with the mode defaulting **off** (no production
change) and step 5+ flip it on behind the gate test.

### Key designs (from the spec, resolved)

- **Reads stay lock-free + safe under a concurrent recycle:** on a `wal_index` hit, read the
  frame with `FrameLog::read_page_if_for(offset, page_id)` — one `pread` of header+page that
  verifies `page_id` **and** `salt` **and** crc. A post-recycle stale offset points at a new
  frame with a different salt ⇒ mismatch ⇒ `None` ⇒ fall through to the main file (which the
  checkpoint made current *before* recycling). No read-path lock.
- **Checkpoint reconciliation:** clear the live `wal_index` **only** on a full recycle;
  recycle is **in-flight-safe** — only when every frame in the log is committed (else apply
  committed frames to main and leave the log, so an uncommitted txn's frames survive).
- **Per-commit flush** lives in `axiomdb-catalog/src/bootstrap.rs`
  (`CatalogBootstrap::ensure_database_roots`, via `CatalogWriter::new`) — NOT axiomdb-sql
  (the catalog is its own crate; memory was stale).

## Dependencies

Must be done first:
- [x] spec-subfase-6c approved (config mode + read fallback B)
- [x] subphases 5, 6a, 6b

Blocks:
- [ ] subphase 7 (full crash suite T1–T7, perf A/B write-up, doublewrite retirement)

## Affected files

- `crates/axiomdb-storage/src/config.rs` — durability/redo mode on `DbConfig`.
- `crates/axiomdb-storage/src/wal_frame.rs` — `read_page_if_for`.
- `crates/axiomdb-storage/src/mmap.rs` — read fallback, frame-only `write_page`, enable redo
  on `create`/`open` per config, checkpoint wal_index clear + in-flight-safe recycle.
- `crates/axiomdb-storage/src/fault_injection.rs` — mirror the in-flight-safe recycle.
- `crates/axiomdb-catalog/src/bootstrap.rs` — gate/drop the per-commit `storage.flush()`.
- `crates/axiomdb-sql/.../` (caller) — pass the config through (if needed).
- `docs-site/src/internals/wal.md`, `user-guide/features/transactions.md`,
  `development/benchmarks.md` — the switch + A/B (close).

⚠️ **Coordination:** step 5 touches **axiomdb-catalog** (and possibly a caller in
axiomdb-sql) — the user works in this broader codebase. Confirm before landing step 5;
steps 1–4 + 6–7 are storage-only / branch-isolated.

---

## Step 1 — `DbConfig` redo/frame-only durability mode

**Goal:** a config that selects frame-only durability (default off this step).
**Files:** `config.rs`.
**Test:** default resolves to off; explicit on resolves to on; round-trips via serde.

```rust
// config.rs — extend DbConfig (mode, not a feature-flag shim: like SQLite journal_mode=WAL)
pub enum RedoMode { Off, FrameOnly }      // Off = today's dual-write + per-commit flush
impl DbConfig { pub fn resolved_redo(&self) -> RedoMode { /* default Off */ } }
```

Commit: `feat(redo-recovery): DbConfig redo/frame-only durability mode (6c step 1)`

---

## Step 2 — `FrameLog::read_page_if_for` + lock-free read fallback

**Goal:** reads survive a concurrent recycle by verifying the frame, falling back to main.
**Files:** `wal_frame.rs`, `mmap.rs` (`read_page` miss path).
**Test (wal_frame):** `read_page_if_for` returns `Some` for a matching `(offset,page_id)`
under the current salt; `None` after `recycle()` (salt changed) or for a wrong page_id.
**Test (mmap):** with redo on, a `wal_index` hit whose frame was recycled out reads the
correct bytes from the main file (no error, no wrong page).

```rust
// wal_frame.rs
/// Reads the frame at `offset` ONLY if it is still a valid frame for `page_id` under the
/// current run salt (one pread of header+page; verifies page_id + salt + crc). `None` if
/// the offset is stale (e.g. recycled) or for a different page — the caller falls back.
pub fn read_page_if_for(&self, offset: u64, page_id: u64)
    -> Result<Option<Box<[u8; PAGE_SIZE]>>, DbError>;

// mmap.rs read_page miss path:
if let Some(frame_log) = &self.frame_log {
    if let Some(frame) = self.wal_index.latest(page_id) {
        if let Some(bytes) = frame_log.read_page_if_for(frame.offset, page_id)? {
            // cache + return
        }
        // else: fall through to mmap (main is current post-checkpoint)
    }
}
```

Commit: `feat(redo-recovery): verified lock-free frame reads with main fallback (6c step 2)`

---

## Step 3 — Checkpoint: clear wal_index on recycle + in-flight-safe recycle

**Goal:** the checkpoint reconciles reads and never truncates an uncommitted txn's frames.
**Files:** `mmap.rs`, `fault_injection.rs` (`checkpoint_frames`); a `WalIndex::clear`.
**Test:** checkpoint with an in-flight (uncommitted) frame present applies committed ones
but does NOT recycle (log non-empty, in-flight frame still readable); checkpoint with all
committed recycles + clears the wal_index (post-checkpoint reads hit main).

```rust
// checkpoint_frames: after apply + fsync main,
//   let all_committed = frame_log.scan()?.iter().all(|f| is_committed(f.txn_id));
//   if all_committed { frame_log.recycle()?; self.wal_index.clear(); }
//   // else: leave the log + index (in-flight tail preserved)
```

Commit: `feat(redo-recovery): in-flight-safe recycle + wal_index clear (6c step 3)`

---

## Step 4 — Frame-only `write_page` + enable redo on open per config

**Goal:** with the mode on, `write_page` stops dual-writing the main file, and
`create`/`open` enable the redo log.
**Files:** `mmap.rs`.
**Test:** with the mode on, after `write_page` the main file is NOT updated (only the frame
+ pool); `read_page` still returns the bytes (from the frame); after a checkpoint the main
file has them. With the mode off, behavior is byte-identical to today (dual-write).

```rust
// write_page_inner redo-on branch: gate the `self.pwrite_bytes(main…)` on the mode —
//   FrameOnly ⇒ skip the main pwrite (append frame + record index + cache only).
// create/open: if config.resolved_redo() == FrameOnly { self.enable_redo_log(db_path)?; }
```

Commit: `feat(redo-recovery): frame-only write_page + enable redo on open (6c step 4)`

---

## Step 5 — Drop/gate the per-commit flush (axiomdb-catalog) ⚠️ coordination

**Goal:** remove the load-bearing `storage.flush()` when redo is on — the actual win.
**Files:** `crates/axiomdb-catalog/src/bootstrap.rs` (`ensure_database_roots`), caller config plumbing.
**Approach:** gate the per-commit `storage.flush()` on the redo mode (skip when FrameOnly —
durability now comes from `commit_durable`/`sync_to_durable`). Keep it when Off.
**Test:** the gating guarantee `test_dirty_open_truncates_unlogged_tables_only` stays green;
T0 green via the production path (frame-only, no per-commit flush).

> ⚠️ Confirm with the user before landing — touches axiomdb-catalog. Stage only these files.

Commit: `feat(redo-recovery): gate the per-commit catalog flush on redo mode (6c step 5)`

---

## Step 6 — Checkpoint trigger (size + clean shutdown)

**Goal:** keep the frame log bounded once frame-only.
**Files:** the commit path (size check → `checkpoint_frames`), `Database` close.
**Test:** after N commits exceeding the threshold, a checkpoint runs (log shrinks); a
clean close leaves a small/empty log.

Commit: `feat(redo-recovery): size + clean-shutdown checkpoint trigger (6c step 6)`

---

## Step 7 — Perf A/B + close

**Goal:** confirm the win and close.
**Verification:** `cargo build --release -p axiomdb-bench-comparison` (macOS) then
`target/release/axiomdb_bench --compare --rows 10000` — autocommit A/B (mode off vs on)
shows the ~2-3× improvement; no read regression. Workspace tests + clippy + fmt; docs
(`wal.md`, `transactions.md`, `benchmarks.md`) + memory.

Final commit: `feat(redo-recovery): close subphase 6c — frame-only switch`

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Stale wal_index offset reads the wrong page after recycle | medium | `read_page_if_for` verifies page_id + salt + crc → fallback to main |
| Recycle truncates an in-flight txn's frames | medium | in-flight-safe recycle (only when all committed) |
| axiomdb-catalog change conflicts with the user's work | medium | confirm first; stage only bootstrap.rs; steps 1–4 are storage-only |
| Frame-only loses data if checkpoint never runs | low | size + clean-shutdown trigger; recovery REDOes from the log on open |
| A/B shows no win | low | the per-commit fsync is the measured dominant cost (`project_insert_perf.md`) |

## Rollback plan

Steps 1–4 default the mode **off** (no production change). The switch is the config mode +
step 5; reverting = set the mode off (or `git reset` step 5). Each step is an isolated commit.

## Estimated effort

Total: ~1.5 days. Steps 1 ~30m, 2 ~1.5h (verified reads), 3 ~1.5h, 4 ~1h, 5 ~1h
(+ coordination), 6 ~1h, 7 ~1.5h (A/B + docs). Implementation effort: **max**.
