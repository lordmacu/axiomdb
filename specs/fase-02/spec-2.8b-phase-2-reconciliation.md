# Spec: 2.8b — Phase 2 Reconciliation And Tracker Cleanup

These files were reviewed before writing this spec:
- `db.md`
- `docs/progreso.md`
- `docs/README.md`
- `docs/fase-2.md`
- `specs/fase-02/spec-btree.md`
- `specs/fase-02/plan-btree.md`
- `crates/axiomdb-index/src/lib.rs`
- `crates/axiomdb-index/src/iter.rs`
- `crates/axiomdb-index/src/tree.rs`
- `crates/axiomdb-index/src/page_layout.rs`
- `crates/axiomdb-sql/src/executor.rs`
- `docs-site/src/internals/btree.md`
- `docs-site/src/development/benchmarks.md`
- `docs-site/src/development/roadmap.md`
- `docs-site/src/development/decisions.md`

## Research synthesis

No external `research/` source is used in this subphase.
This is a codebase-reconciliation task driven by the implemented AxiomDB code and
the project documentation already checked into the repo.

## What to build (not how)

Phase 2 documentation and tracker state must reflect the B+ Tree that AxiomDB
actually ships today.

This subphase reconciles five facts that are already true in code:

1. `RangeIter` advances between leaves by re-traversing from the root; it does
   not follow `next_leaf`.
2. The `next_leaf` pointer still exists on disk, but its CoW-consistent
   maintenance remains deferred to Phase 7.
3. The internal-node `rotate_right` stale-byte bug is fixed.
4. The `SessionContext` stale `root_page_id` cache bug after index root changes
   is fixed.
5. Phase 2 docs/specs must no longer describe deleted files, old layouts, or
   old benchmark explanations that contradict the current implementation.

The result of this subphase is an auditable, implementation-aligned Phase 2
record: `docs/progreso.md`, `docs/fase-2.md`, `docs/README.md`, the crate docs,
docs-site development docs, and the historical Phase 2 spec/plan all agree on
what is complete, what was fixed, and what is still deferred.

## Inputs / Outputs

- Input:
  - current Phase 2 implementation in `crates/axiomdb-index`
  - current tracker/docs/specs that still contain stale Phase 2 statements
- Output:
  - Phase 2 tracker entries reflect only real pending debt
  - Phase 2 docs point to the real existing files and current behavior
  - historical Phase 2 spec/plan remain usable and no longer describe obsolete
    range-scan behavior, obsolete files, or obsolete layout numbers
- Errors:
  - none at runtime
  - any behavior that is not implemented today must remain deferred, not be
    marked completed by documentation cleanup

## Use cases

1. A contributor reads `docs/progreso.md`.
   They see `next_leaf` as the only real deferred Phase 2 gap, while the
   `rotate_right` and stale `root_page_id` bugs are clearly marked resolved.

2. A contributor opens `specs/fase-02/spec-btree.md` or `plan-btree.md`.
   They do not get told that `RangeIter` walks a leaf linked list, that
   `node.rs` exists, or that `ORDER_LEAF` still uses the obsolete 12-byte RID
   layout.

3. A maintainer reads `docs/README.md`.
   The Phase 2 entry points to the real `docs/fase-2.md` file and shows the
   correct completion state.

4. A reader checks benchmark rationale.
   `docs-site/src/development/benchmarks.md` explains the measured range-scan
   result using the real tree-retraversal iterator rather than a non-existent
   `next_leaf` fast path.

## Acceptance criteria

- [ ] `docs/progreso.md` no longer leaves the fixed `rotate_right` bug and the fixed stale `root_page_id` cache bug as open `[ ]` items
- [ ] `docs/progreso.md` still keeps the `next_leaf` CoW gap explicitly deferred to Phase 7
- [ ] `docs/fase-2.md` distinguishes between deferred `next_leaf` maintenance and the two already-fixed bugs
- [ ] `docs/README.md` points to the real `docs/fase-2.md` file and does not claim Phase 2 is still pending
- [ ] `crates/axiomdb-index/src/lib.rs` no longer claims that range scan uses a leaf linked list
- [ ] `docs-site/src/development/benchmarks.md` explains Phase 2 range-scan behavior using root re-traversal, not `next_leaf` pointer chasing
- [ ] `specs/fase-02/spec-btree.md` no longer claims `BTree::range(...)` traverses the leaf linked list
- [ ] `specs/fase-02/plan-btree.md` no longer references obsolete implementation details such as `node.rs`, `ORDER_LEAF = 211`, or iterator logic based on `next_leaf`
- [ ] No updated Phase 2 document incorrectly marks the deferred `next_leaf` problem as solved

## Out of scope

- Implementing CoW-safe `next_leaf` maintenance
- Changing B+ Tree algorithms or page layout
- New Phase 2 benchmarks or benchmark numbers
- MVCC or epoch reclamation work from Phase 7
- Reopening already-closed functional work from Phase 2

## Dependencies

- `docs/progreso.md` — tracker source of truth for pending vs fixed items
- `docs/fase-2.md` — Phase 2 completion summary
- `docs/README.md` — index of phase docs
- `crates/axiomdb-index/src/iter.rs` — actual range iterator behavior
- `crates/axiomdb-index/src/tree.rs` — actual `rotate_right` fix
- `crates/axiomdb-index/src/page_layout.rs` — actual leaf layout constants
- `crates/axiomdb-index/src/lib.rs` — public crate-level Phase 2 summary
- `crates/axiomdb-sql/src/executor.rs` — actual `ctx.invalidate_all()` fix for stale `root_page_id`
- `specs/fase-02/spec-btree.md` — historical Phase 2 spec to reconcile
- `specs/fase-02/plan-btree.md` — historical Phase 2 plan to reconcile
- `docs-site/src/development/benchmarks.md` — stale range-scan explanation to update
- `docs-site/src/development/roadmap.md` — already-aligned reference text to preserve
- `docs-site/src/development/decisions.md` — already-aligned reference text to preserve

## ⚠️ DEFERRED

- CoW-consistent `next_leaf` maintenance remains deferred to Phase 7
  - `RangeIter` is already correct without it
  - this subphase must keep that debt visible instead of hiding it
