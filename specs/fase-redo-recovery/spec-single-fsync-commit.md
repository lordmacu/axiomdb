# Spec: single-fsync commit — commit marker in the frame log (PostgreSQL single-WAL)

Phase: redo-recovery (project B) — commit-path durability optimization
Task: Collapse the **2 fsyncs per durable commit → 1** under frame-only redo, by
recording the commit decision *inside the frame log* instead of cross-referencing a
separate logical WAL — AxiomDB's version of PostgreSQL's single-WAL commit
(`research/postgres/src/backend/access/transam/xact.c:1317` `RecordTransactionCommit`,
one `XLogFlush`).
Status: approved (2026-05-22) — encoding open questions delegated to the plan.

## Context

Under frame-only redo (the embedded default), a durable commit at `synchronous=FULL`
(`Strict`) pays **two** fsyncs, by write-ahead order:

1. `commit_durable` → `storage.sync_frame_log()` — the txn's data frames durable first
   (`crates/axiomdb-wal/src/txn_begin_commit.rs:103`).
2. `commit` → `wal.commit_data_sync()` — the logical `Commit` record durable
   (`txn_begin_commit.rs:161`).

Two fsyncs exist because there are **two files** and recovery cross-references them: a
frame is applied iff its `txn_id` has a `Commit` in the **logical WAL**
(`wal_frame.rs:13`; `FrameLog::build_index(is_committed)` `wal_frame.rs:592`; recovery
builds the committed set by scanning `EntryType::Commit` and hands
`|t| committed.contains(&t)` to `redo_committed_frames` `recovery.rs:565`). PostgreSQL
has one WAL, so commit is one flush. Measured cost of the extra fsync: FULL autocommit
≈ 260 ops/s ≈ 3.9 ms/commit ≈ 2 APFS fsyncs.

Two facts make the logical WAL removable from the *durable commit path* under
frame-only:
- **Recovery never applies uncommitted frames** — `build_index` skips them
  (`wal_frame.rs:589-591`), so there is no physical-undo dependency on the logical WAL.
- **Live `ROLLBACK` uses in-memory `conn_txn.undo_ops`** (`txn_rollback.rs:150-168`),
  and its `Rollback` WAL entry is explicitly "informational … No fsync". So the durable
  logical WAL's *only* remaining job under frame-only is the recovery committed-predicate
  — which this spec moves into the frame log.

## Goal

Make frame-only crash recovery determine committed-ness from the **frame log alone**
(a durable, frame-log-resident commit marker per committed txn), so a durable commit
needs **one** fsync (the frame log) instead of two.

## Non-goals

- **Not** changing the `redo=off` path — it keeps the logical-WAL committed predicate
  and its existing fsync (this spec only touches frame-only).
- **Not** merging the two logs into one physical WAL (Approach B) — separate spec.
- **Not** the no-undo-log / MVCC-visibility rollback model (Approach C) — separate spec.
- **Not** improving `synchronous=NORMAL` autocommit — NORMAL has no per-commit fsync;
  this lever targets FULL/Strict only. (NORMAL must **not regress** — see budget.)
- **Not** group commit / pipeline changes (the existing deferred-pipeline path stays).

## Behavior

### What changes

1. **Commit marker in the frame log.** At a Strict commit under frame-only, after the
   txn's data frames, the engine appends a durable **commit marker** identifying the
   committed `txn_id`. The marker lives in the frame log so a single `sync_frame_log()`
   makes the data frames *and* the commit decision durable together (write-ahead order
   preserved: data frames precede the marker in the gap-free durable prefix).
2. **Recovery committed-predicate from the frame log.** Under frame-only, the set fed to
   `build_index` / `redo_committed_frames` is derived from frame-log commit markers, not
   from a logical-WAL scan. A frame is applied iff its `txn_id` has a durable commit
   marker at an offset within the recovered gap-free prefix.
3. **One fsync at commit.** `commit_durable` under frame-only appends the marker and
   calls `sync_frame_log()` **once**; it does **not** call `commit_data_sync()`.
4. **Logical WAL → flush-no-sync under frame-only.** The logical `Commit`/row records are
   still written (kept for live rollback bookkeeping and any non-frame-only consumer) but
   flushed to the OS only, never fsync'd on the commit path.

### Public API (internal — within axiomdb-wal / axiomdb-storage)

```rust
// axiomdb-storage::wal_frame::FrameLog — append a durable commit marker for `txn_id`.
// Returns the marker's byte offset. Must be ordered AFTER the txn's data-frame appends.
pub fn append_commit_marker(&self, txn_id: u64, lsn: u64) -> Result<u64, DbError>;

// FrameLog::build_index already takes the committed predicate; recovery now builds that
// predicate from the frame log's own markers under frame-only:
//   let committed = frame_log.committed_txns_from_markers()?; // BTreeSet<u64> or similar
// (exact shape decided in /plan-task; see Open questions)

// axiomdb-storage StorageEngine — surface the marker append + the marker-derived
// committed set so axiomdb-wal's commit_durable / recovery can use them without a
// logical-WAL fsync. Exact trait methods decided in /plan-task.
```

### Semantics

- **Precondition (marker append):** every data frame the committing txn produced has
  already been appended (reserved) to the frame log.
- **Postcondition (Strict commit):** after `commit_durable` returns Ok, the txn's data
  frames AND its commit marker are within the frame log's `durable` prefix; exactly one
  fsync occurred; `max_committed` advanced.
- **Invariant (write-ahead):** for any txn `T` whose commit marker is in the durable
  prefix, *all* of `T`'s data frames are also in the durable prefix (the marker is
  appended after them and `sync_to_durable` only advances over a gap-free prefix).
- **Invariant (recovery equivalence):** the committed set derived from frame-log markers
  under frame-only equals the set the old logical-WAL scan would have produced for the
  same durable history (modulo the last unsynced txns a crash legitimately drops).

### Error cases

| Input | Expected | Notes |
|-------|----------|-------|
| Torn/partial commit marker at the tail | treated as **not committed** | frame CRC+salt guard ends the valid prefix; `T`'s frames are not applied |
| Marker present but a `T` data frame missing from the prefix | impossible by the write-ahead invariant; if detected → recovery error, not silent apply | guarded by the gap-free prefix + a debug assert |
| `redo=off` | unchanged path | logical-WAL predicate + its fsync retained |

## Edge cases

- [ ] Read-only / empty txn (no data frames, `undo_ops` empty) → no marker, no fsync
      (current `flush_no_sync` read-only path is retained).
- [ ] Txn wrote frames across multiple pages, then commits → one marker covers the txn;
      recovery applies all of `T`'s frames.
- [ ] Crash after data frames fsync'd but before the marker → `T` not committed → its
      frames are skipped (correct: the commit never became durable).
- [ ] Crash mid-marker write → CRC/salt guard → not committed.
- [ ] Multi-writer interleaving: frames+markers from different txns interleave; each
      marker self-identifies `txn_id` (matches the existing per-frame `txn_id` model).
- [ ] Frame-log **recycle/checkpoint**: after a checkpoint applies committed frames to
      main and recycles the log, markers for already-applied txns must not resurrect or
      block recycle (marker lifetime = same as the frames it commits).
- [ ] Old frame-log file written **before** this format (no markers) → must still
      recover: fall back to the logical-WAL predicate when no markers are present
      (version/sentinel detection) OR a one-way format-version bump (decide in plan).
- [ ] `SET synchronous` changing within a run (NORMAL↔FULL) → markers written regardless
      of mode (so recovery is uniform); only the *fsync* is mode-gated. (Confirm no
      NORMAL volume regression — see budget; this drives the compact-marker requirement.)

## On-disk format

The commit marker is a **compact** frame-log record (NOT a full 16 KiB page frame). A
full-frame marker would add ~16 KiB of WAL volume per commit, which **regresses NORMAL
autocommit** (NORMAL gets no fsync benefit) — forbidden by the budget. The marker reuses
the 36-byte frame header shape with a sentinel discriminator:

```
Commit marker (compact):
  offset  size  field      description
  0       8     page_id    sentinel COMMIT_MARKER (e.g. u64::MAX) ⇒ "not a page frame"
  8       8     lsn        marker LSN (monotonic, for the gap-free prefix)
  16      8     txn_id     the committed transaction
  24      8     salt       run identity (stale-frame guard, as today)
  32      4     frame_crc  crc32c over bytes [0..32]
  (no page payload)
```

The frame log therefore becomes **variable-stride**: a record is a full page frame
(`FRAME_HDR_SIZE + PAGE_SIZE`) or a compact marker (`FRAME_HDR_SIZE`), distinguished by
`page_id == COMMIT_MARKER`. `append`, `scan`, `build_index`, the lock-free
offset-reservation (`fetch_add`), and `sync_to_durable`'s gap-free-prefix bookkeeping
must all advance by the actual record length, not a fixed `FRAME_SIZE`.

**Compatibility rule:** bump the file-header `VERSION` (`wal_frame.rs:33`) to gate the
variable-stride reader; an old (v1) log has no markers → recovery uses the logical-WAL
predicate for it.

## Performance budget

| Metric | Target |
|---|---|
| **FULL autocommit throughput** | **≥ +80%** (≈2×; one fsync instead of two) |
| NORMAL autocommit throughput | within **±2%** (no regression — the `--compare` mode) |
| insert_batch (FULL & NORMAL) | within ±2% |
| reads (full_scan / point_lookup / range_scan) | within ±2% |
| recovery time | not worse than the logical-WAL scan |

Measure with an interleaved OFF/ON A/B on macOS native (the only trustworthy method;
macOS is ±60% cross-run). Reference: FULL autocommit baseline ≈ 260 ops/s.

## Dependencies

- Depends on: frame-only redo (6c), the frame log (`wal_frame.rs`), `WalIndex`
  (`last_commit_lsn`/`set_last_commit_lsn` are reusable), `sync_to_durable`/gap-free
  prefix (6a), the crash suite (`integration_redo_crash_suite.rs`), `IntegrityChecker`.
- Blocks: nothing. Independent of frame-hole-skipping (paused).

## Open questions

- [ ] Marker encoding: compact variable-stride record (recommended, no NORMAL volume
      regression) vs a header-bit on the last data frame (avoids variable stride but
      needs a commit-time rewrite of an appended frame). → resolve in `/plan-task`.
- [ ] Committed-set shape passed to `build_index` from markers (full `BTreeSet<u64>` vs a
      committed-`txn_id` high-water + per-frame check). 
- [ ] Old-format fallback (sentinel detection at scan) vs hard `VERSION` bump.
- [ ] Does the checkpointer need to advance a "markers durable through LSN" so recycle
      can drop markers safely? (Likely reuses `last_commit_lsn`.)

## Done criteria

- [ ] Frame-only recovery derives committed-ness from frame-log markers; **no logical-WAL
      read** needed under frame-only (redo=off still uses it).
- [ ] `commit_durable` under frame-only performs **exactly one** fsync (assert/counter in
      a test).
- [ ] Crash suite `integration_redo_crash_suite.rs` **T1–T7 green** with commit-ness from
      the frame log.
- [ ] `IntegrityChecker` clean after autocommit / batch / random-key / crash-mid-txn
      workloads.
- [ ] `redo=off` unaffected — existing `clustered_recovery` + recovery suites green.
- [ ] **No regression**: NORMAL autocommit, insert_batch, and reads within ±2% (A/B);
      **FULL autocommit ≥ +80%**.
- [ ] `cargo nextest run --workspace` (Lima) green; clippy + fmt clean.
- [ ] Docs `internals/wal.md`: commit-marker-in-frame-log, the PG single-WAL parallel,
      the 2→1 fsync change, redo=off vs frame-only.

## References

- `research/postgres/src/backend/access/transam/xact.c:1317` `RecordTransactionCommit`
  (single commit record + one `XLogFlush`).
- `crates/axiomdb-wal/src/txn_begin_commit.rs:103,161` — the two fsyncs today.
- `crates/axiomdb-storage/src/wal_frame.rs:592` `build_index(is_committed)`,
  `:159-165` `last_commit_lsn`.
- `crates/axiomdb-wal/src/recovery.rs:155,565` — committed-set scan + `redo_committed_frames`.
- `crates/axiomdb-wal/src/txn_rollback.rs:150-168` — live rollback uses in-memory undo.
- `specs/fase-redo-recovery/spec-frame-hole-skipping.md` — the paused volume lever; its
  "Measure-first RESULT" is why this commit-path lever was chosen.

## Recommended effort for /plan-task

**high** — the design is fully specified, but the plan must sequence a TDD path with the
crash suite (T1–T7) as the gate at every step, handle the variable-stride format change
carefully, and keep the redo=off path bit-identical. (Implementation itself is **max** —
crash-recovery safety-critical.)
