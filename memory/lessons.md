# Lessons Learned

## 2026-04-10 — Phase 11.x

- **TOAST refcounts belong on the owned BLOB chain, not clustered overflow.** Use versioned `ABOB` header only for TOAST/BLOB-owned chains; clustered row overflow stays non-refcounted so Phase 39 physical descriptors remain stable.
- **Buffer pool eviction: scan LRU candidates at most once per attempt.** If all candidates are pinned, exit; don't spin. Resume enforcement when a page is unpinned.
- **Large wire values → use `COM_STMT_SEND_LONG_DATA`, not `COM_QUERY` literals.** `COM_QUERY` stack-overflows tokio worker around 7 KB. Separate BLOB wire correctness from long-literal parser hardening.
- **`decode_row_masked` must detect TOAST sentinels before skipping variable payloads.** Read u24, check for sentinel, consume 12-byte pointer; don't interpret `0xFF_FFFE` as inline payload length.
- **Split roadmap entries when a feature combines a usable scope + deferred storage work.** For 11.4: text-backed Native JSON; JSONB layout and GIN indexing explicitly deferred to 11.16/11.17.

## 2026-04-09 — B-tree safe descent (Phase 39)

- **Delete safe descent needs stricter predicate than insert.** Insert: "child has room for one more key" sufficient. Delete: must also guarantee child won't enter CoW rebalance path. Simplest: child is leaf with > MIN_KEYS_LEAF keys.
- **Clustered B-tree never allocates new pid on rebalance** (left page reused); safe-descent predicate is purely byte occupancy.
- **`cargo test --workspace` is the correct final gate command.** Linux is the faster validation baseline.

## 2026-04-03 — Clustered maintenance (Phase 39)

- **Heap→clustered migration contract:** build new clustered root first, flush before any catalog pointer change, swap metadata, queue old pages through deferred free (never inline `free_page` during swap).
- **Clustered VACUUM predicate:** use physical row existence, not snapshot visibility, to decide secondary bookmark cleanup.

## 2026-04-02 — Clustered storage invariants (Phase 39)

- **Clustered undo keys = PK bytes + exact row image.** Never `(page_id, slot_id)` — clustered pages defragment and relocate rows freely.
- **Tombstone-reuse INSERT → log as clustered update undo, not insert undo.** Rollback must restore the old tombstone.
- **Clustered SQL UPDATE must capture exact old row image before mutation.** Secondary undo needs both halves: delete new bookmark + reinsert old bookmark.
- **Variable-size clustered split by encoded bytes, not key count.** Left page keeps old ID; only right sibling allocated.
- **Clustered overflow: keep PK and RowHeader inline, spill only row tail.** Not a full TOAST — no compression, no generic BLOB references in Phase 39.
- **Clustered secondary key = secondary_logical_key ++ missing_PK_columns.** Never depends on heap RecordId.
- **Crash recovery: keep committed clustered roots separate from all-seen roots.** Rolled-back transactions must not poison the root map.
- **Covering IndexOnlyScan on clustered: normalize back to clustered-aware access** until a true index-only path exists. Visibility comes from clustered row header, not heap slot.

## 2026-03-29 — Test structure

- **Split integration tests by execution path, not size alone.** ~1000 lines is a watch signal, not an automatic trigger. Files around subphase boundaries produce the cleanest ownership.

## 2026-03-26 — Implementation process

- **Incremental validation:** start with `cargo test -p <touched-crate>`, expand to dependents only when public API, on-disk format, WAL/recovery, SQL semantics, or wire behavior changed.
- **Spec discipline:** read codebase files before writing the spec. List real files reviewed. Dependencies must name real files, not just modules.
- **Plan must have zero loose ends:** return types, error paths, ownership boundaries, exact call sites — all decided before implementation starts.
- **Large Rust module splits:** use `include!` in first step to preserve private visibility while improving reviewability.
- **Separate transport lifecycle from SQL session state.** `COM_RESET_CONNECTION` tests the boundary.
- **DML perf has two diagnoses:** candidate discovery vs index maintenance — profile both independently.
- **UPDATE index skip requires two conditions:** RID stable AND logical key / predicate membership unchanged.
- **Research citations:** cite exact file paths from `research/`, not just "PostgreSQL" or "MariaDB". Use borrow/reject/adapt mindset.
- **Critical subphases (correctness, durability, WAL, crash recovery):** review all relevant engines in `research/`, not just the obvious one. Correctness before speed.
- **Default choice:** pick best + most robust solution over quick/minimal hacks. If semantics unclear, consult `research/{postgres,mariadb,duckdb}` source and cite the exact path.
