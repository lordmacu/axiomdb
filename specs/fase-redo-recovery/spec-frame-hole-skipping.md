# Spec: frame-hole-skipping — omit the free-space hole from redo frames (PostgreSQL REGBUF_STANDARD)

Phase: redo-recovery (project B) — WAL-volume optimization
Task: Borrow PostgreSQL's full-page-image hole elision for AxiomDB's page-frame
redo log, to cut WAL volume (most impactful for autocommit, our worst write gap).
Status: PAUSED — measure-first gate FAILED for *throughput* (2026-05-22). The hole
fraction is large (autocommit 57.5%, batch 33.3%) but it does NOT convert to
autocommit throughput on macOS APFS (FULL ~0%: fsync is fixed-cost; NORMAL: the
16KB frame pwrite is a tiny share of the ~22-34µs/commit, executor/B-tree-bound).
Hole-skipping is a WAL-VOLUME win (disk/checkpoint/recovery/bandwidth-bound disks),
NOT a write-speed lever. Foundation (`free_hole` + tests) stays; the format change
is deferred. See "Measure-first RESULT" below. Pivot: the autocommit cost is the
**2 fsyncs/commit** (frame log + logical WAL) vs PG's single WAL — a logging
consolidation, not hole elision.

## Progress / Handoff (2026-05-22)

### ✅ DONE — Step 1: hole computation + safety validation (read-only, no risk)

The safety crux (are the hole bounds correct?) is implemented and **proven**, with
**no on-disk format or write-path change yet** — so nothing here can corrupt data.

- **`clustered_leaf::free_hole(page) -> (offset, len)`** in
  `crates/axiomdb-storage/src/clef_access.rs` — returns the absolute byte range of
  the contiguous free hole `[ptr_array_end .. cell_content_start]` (PG's
  `pd_lower..pd_upper`). `len == 0` for a full page; `offset >= HEADER_SIZE`.
- **Property tests** in `crates/axiomdb-storage/tests/integration_clustered_tree.rs`:
  - `clustered_leaf_free_hole_is_entirely_free_space` — zeroing the computed hole
    + recomputing the checksum preserves **every cell byte-for-byte** (a wrong
    range would zero live data → this is the corruption guard).
  - `clustered_leaf_free_hole_is_zero_when_full` — a balance_quick-packed leaf has
    ~no hole (→ full-frame fallback, no elision).
- Commits: spec `db03464f`, foundation `21a945b8` (both pushed to
  `origin/fase-redo-recovery`). Storage clustered-tree suite 16/16 green.

### ⏳ REMAINING — the format/write-path change (safety-critical, gated)

1. **Per-type hole dispatch** — extend beyond ClusteredLeaf: a
   `Page::redo_free_hole() -> Option<(offset,len)>` dispatching by `page_type`
   (ClusteredInternal + heap/Data slotted layouts; `None` for Meta/Overflow/Free/
   Index). Each new type needs its own `free_hole` + the same property test.
2. **`zero-the-hole-on-write`** in `write_page` (before `update_checksum`) — the
   **ONLY read-safe design** (see the table below): zero the hole so the in-memory
   page, the main file, and the frame all agree, and reconstruction is
   byte-identical with **no checksum recompute** (no read regression). Do NOT use
   recompute-on-reconstruct (it regresses wal-index reads).
3. **Frame format** (`wal_frame.rs`) — add `hole_offset:u16 + hole_len:u16` to the
   frame header (`FRAME_HDR_SIZE` 36→40); payload = `page[0..off] + page[off+len..]`;
   `hole_len==0` sentinel keeps old `.wf` readable. `frame_crc` over the logged
   segments only.
4. **Reconstruct** (recovery REDO + wal-index read) — segment1 + zeros + segment2;
   no recompute (the source page's hole is already zero from step 2).
5. **Crash/integrity gate** — the crash suite `integration_redo_crash_suite.rs`
   (T1–T7) green WITH hole-skipping + `IntegrityChecker` clean after
   autocommit/batch/random/delete; round-trip property test
   (`write → frame → reconstruct == original`). `redo=Off` must stay unaffected.
6. **Measure-first** — confirm the autocommit win (~10-30% est.) before/after; abort
   if < ~10%. The hole fraction is ~56% for autocommit, ~12% for the append-batch.

### 🛑 Measure-first RESULT (2026-05-22) — gate FAILED for throughput

Measured with a gated read-only counter (`hole_measure::record`, reuses the proven
`free_hole`) + a throwaway hole-skip pwrite A/B (both reverted after measuring;
never shipped). Frame-only redo forced (`AXIOMDB_BENCH_REDO=1`).

- **Hole fraction (definitive, hardware-independent, Lima == macOS):** autocommit
  **57.5%** (2.83/4.93 MB over 301 leaf frames), batch **33.3%** (4.31/12.96 MB
  over 791 frames). Batch is *worse* than the spec's ~12% estimate: the rightmost
  leaf is re-framed partially as it fills (~2.2 frames/leaf), not "once when full".
- **Autocommit throughput A/B (macOS APFS, interleaved OFF/ON):**
  - `synchronous=FULL` (fsync/commit): **−1.2%** (clean). At ~260 ops/s a commit is
    ~3.9ms = 2 APFS fsyncs; APFS fsync is fixed-cost, so a smaller frame ≈ no win.
  - `synchronous=NORMAL` (deferred fsync, the vs-SQLite mode): throwaway shows −24%
    but that is a **sparse-stride artifact** (the throwaway keeps `FRAME_SIZE` stride
    → sparse file → slower APFS writes); cleanly, the frame pwrite is a tiny share of
    the ~22-34µs/commit, so the real win is < 10% regardless.

**Verdict:** per the gate ("abort if < ~10%"), the format change is **NOT** justified
as a throughput optimization. It remains valid as a *volume* optimization (−57%/−33%
WAL bytes) for disk footprint, checkpoint/recovery speed, and bandwidth-bound
(cloud/network) storage where fsync scales with bytes. The autocommit throughput
lever is elsewhere: **collapse the 2 fsyncs/commit → 1** (consolidate the frame log
and the logical WAL toward PostgreSQL's single-WAL model). See
`memory/lessons.md` and the fsync-consolidation brainstorm.

## Context

AxiomDB's redo frame log writes a **full 16 KiB page image** per frame
(`wal_frame.rs:42`, `FRAME_SIZE = FRAME_HDR_SIZE + PAGE_SIZE`). PostgreSQL's WAL
omits the free-space "hole" of a standard page (`pd_lower..pd_upper`,
`xloginsert.c:729` `REGBUF_STANDARD`) — it logs `[0..lower] + [upper..end]` and
zero-fills the hole on replay. A slotted page that is half-full thus costs PG
~half the WAL volume AxiomDB pays.

AxiomDB pages have the same slotted structure: a cell-pointer array growing up
from the header and cell content growing down from the end, with a contiguous
**free hole** in between (`cell_content_start` is the hole's upper bound; the
pointer-array end is the lower bound). So the technique is directly applicable.

Measured/reasoned win (the hole fraction is the geometry of a filling leaf):
- **autocommit ~56% hole** (each commit re-frames the leaf at an increasing fill
  1→~140 cells; average ~half-full) → ~half the autocommit WAL volume.
- **append-batch ~12% hole** (the `balance_quick` append split packs leaves to
  100% before framing once) → little benefit.
- Random inserts / catalog / secondary-index pages: variable, often large holes.

The fsync at a commit is *partly* fixed-cost, so a ~50% smaller frame yields an
estimated **~10–30% autocommit throughput** improvement (to be measured
precisely in Step 1), plus a faster checkpointer (less to apply) and less disk.

## Goal

Cut redo-frame WAL volume by eliding the contiguous free hole of slotted pages,
**without regressing reads** and without weakening crash recovery.

## Non-goals

- WAL compression of the logged segments (PG `wal_compression`) — separate,
  optional, CPU-trade-off follow-up.
- Minimal-WAL for new-in-txn relations (PG `RelationNeedsWAL`) — a different,
  narrower bulk-load optimization; separate spec.
- Eliding freeblocks (freed cells inside the content area) — only the single
  contiguous middle hole is skipped; freeblocks are logged in full.
- Non-slotted pages (Meta, Overflow, Free, Index) — logged full (no hole).

## Behavior

### Read-safety decision (the crux)

The page checksum (`page.rs:58`) covers the **whole body `[64..PAGE_SIZE]`,
including the hole**. Three candidate designs; only one is read-safe:

| Design | Read regression? | Verdict |
|---|---|---|
| Recompute checksum on every reconstruct | YES — crc32c 16 KiB per wal-index read of an un-checkpointed frame | ❌ violates "don't regress reads" |
| Frame a zeroed copy (compute checksum once at write) | no | copy+crc per frame may eat the I/O savings |
| **Zero the hole on `write_page`, then skip it** | **no** | ✅ chosen |

**Chosen: zero-the-hole-on-write.** Before `update_checksum`, `write_page` zeroes
the contiguous free hole. Then the in-memory page, the main-file page, and the
frame source all agree (hole = zeros, checksum over zeros). The frame skips the
(zero) hole; reconstruction zero-fills → **byte-identical** to the original →
checksum valid with **no recompute** → no read regression.

### Page hole introspection

```rust
// page.rs (or per-type): the contiguous free hole [offset, len) within the body,
// or None for page types without a slotted layout. offset >= HEADER_SIZE always
// (so the header — magic, type, checksum, LSN — is never elided).
pub fn redo_free_hole(&self) -> Option<(usize, usize)>;
```
- ClusteredLeaf / ClusteredInternal: `[ptr_array_end .. cell_content_start]`
  (absolute), via the existing clef/internal header fields.
- Data (heap, slotted): the analogous slot-array-end .. tuple-content-start.
- Meta / Overflow / Free / Index: `None`.

**Invariant (safety-critical):** the returned range contains ONLY free space —
never a live cell, pointer, the header, or a freeblock-with-data. A wrong range
zeroes live data = corruption. Hence the heavy test gate below.

### Frame format

`FrameHeader` gains `hole_offset: u16` + `hole_len: u16` (4 bytes; `FRAME_HDR_SIZE`
36 → 40, or pack into reserved space). `hole_len == 0` ⇒ full page (today's
format / non-slotted pages). The on-disk frame payload is
`PAGE_SIZE - hole_len` bytes: `page[0..hole_offset]` then
`page[hole_offset+hole_len..PAGE_SIZE]`. A format-version bump or the
`hole_len==0` sentinel keeps old `.wf` files readable.

### Reconstruction (recovery REDO + wal-index read)

`frame → page`: copy segment 1 into `[0..hole_offset]`, zero
`[hole_offset..hole_offset+hole_len]`, copy segment 2 into the rest. The result
is byte-identical to the (hole-zeroed) source page; its checksum (logged in
segment 1) is already valid. No recompute.

## Edge cases

- [ ] Full page (no hole) → `hole_len==0` → full-page frame (today's path).
- [ ] Non-slotted page (Meta/Overflow/…) → `None` → full frame.
- [ ] Page mid-defragment / with freeblocks → hole = the contiguous gap only;
      freeblocks logged in full (in segment 2).
- [ ] `hole_offset` must be ≥ `HEADER_SIZE` and the range within the body.
- [ ] Reconstruct of an OLD-format frame (`hole_len==0`) → full page.
- [ ] Crash mid-write of a hole-skipped frame → frame CRC (over the logged
      segments) catches a torn frame → scan stops (existing salt/CRC guard).
- [ ] A page whose hole bounds are momentarily inconsistent must NOT be framed
      with a wrong hole (assert/verify before zeroing).

## Performance budget

| Metric | Target |
|---|---|
| autocommit (300) throughput | ON ≥ +10% vs full-frame (measure Step 1 first) |
| reads (full_scan / point_lookup / range_scan) | within ±2% (no regression) |
| insert_batch | within ±3% (append packs full → little change) |
| frame-log bytes (autocommit) | ~40–60% smaller |

## Dependencies

- Depends on: the redo frame log (subphases 3–6f), the crash suite
  (`integration_redo_crash_suite.rs`), `IntegrityChecker`.
- Blocks: nothing.

## Open questions (resolved)

- **Zero-on-write vs recompute-on-read?** → zero-on-write (only read-safe option).
- **Where does the hole come from?** → `Page::redo_free_hole()`, per-type, the
  contiguous gap only.
- **Old `.wf` compat?** → `hole_len==0` sentinel = full page.

## Done criteria

- [ ] **Step 1 (measure-first):** a gated dry-run counter confirms the hole
      fraction (autocommit vs batch) and the autocommit throughput win BEFORE
      the format change. If the win is < ~10% autocommit, STOP and document.
- [ ] `Page::redo_free_hole()` per page type, with a property test:
      `for every page op → redo_free_hole range is entirely free (zeroing it
      preserves all cells/pointers + IntegrityChecker clean)`.
- [ ] `write_page` zeroes the hole before `update_checksum` (all write paths).
- [ ] Frame append skips the hole; reconstruct zero-fills; **round-trip property
      test**: `write → frame → reconstruct == original (hole-zeroed) byte-for-byte`.
- [ ] Crash suite T1–T7 green WITH hole-skipping (redo of hole-skipped frames).
- [ ] `IntegrityChecker` clean after large autocommit + batch + random + delete
      workloads (the corruption guard).
- [ ] No read regression (interleaved A/B: full_scan/point_lookup/range_scan).
- [ ] `cargo nextest run --workspace` green; clippy + fmt clean.
- [ ] docs `internals/wal.md` (frame hole elision, PG REGBUF_STANDARD parallel).

## Risk register

| Risk | Severity | Mitigation |
|---|---|---|
| Wrong hole bounds zero live data → **silent corruption** | **critical** | per-type `redo_free_hole` property test + IntegrityChecker after every workload + the round-trip byte-identical test; start with ClusteredLeaf only, expand per-type |
| Read regression | high | zero-on-write design (no recompute); A/B reads |
| write_page hot-path cost (memset) | low | memset ≤ hole; only when a hole exists; ≪ I/O saved |
| Win below estimate (fsync fixed-cost) | medium | Step 1 measures before the format change; abort if <10% |

## References

- `research/postgres/src/backend/access/transam/xloginsert.c:729` —
  REGBUF_STANDARD hole elision (`hole_offset`/`hole_length`).
- `crates/axiomdb-storage/src/wal_frame.rs:42` — current full-page frame.
- `crates/axiomdb-storage/src/page.rs:58` — checksum covers the body incl. hole.
- `crates/axiomdb-storage/src/clef_access.rs` — clustered-leaf slotted layout.
- `crates/axiomdb-wal/tests/integration_redo_crash_suite.rs` — the gate.
