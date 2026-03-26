# Plan: 2.8b — Phase 2 Reconciliation And Tracker Cleanup

## Files to create / modify

- `docs/progreso.md` — convert fixed Phase 2 bug notes into resolved state and keep only the real deferred gap open
- `docs/fase-2.md` — reconcile the Phase 2 summary with the actual fixed bugs and deferred debt
- `docs/README.md` — fix the Phase 2 entry path and status
- `crates/axiomdb-index/src/lib.rs` — align crate-level docs with tree-traversal range scan
- `docs-site/src/development/benchmarks.md` — fix the stale explanation of Phase 2 range-scan performance
- `specs/fase-02/spec-btree.md` — update the historical spec to match the as-built Phase 2 implementation
- `specs/fase-02/plan-btree.md` — update the historical plan to remove obsolete files and stale iterator/layout claims

## Algorithm / Data structure

This is a reconciliation pass, not a runtime feature.

Treat the implemented code as the authority and update every stale Phase 2
artifact against that authority:

```text
authority_1 = crates/axiomdb-index/src/iter.rs
authority_2 = crates/axiomdb-index/src/tree.rs
authority_3 = crates/axiomdb-index/src/page_layout.rs
authority_4 = crates/axiomdb-sql/src/executor.rs

for each Phase 2 doc/spec/tracker artifact:
    read current statement
    compare against the authorities
    if statement describes implemented behavior:
        keep it
    else if statement describes fixed historical bug:
        keep it only as resolved historical context
    else if statement describes still-missing behavior:
        keep it explicitly deferred
    else:
        rewrite or remove it
```

The key reconciliation rules are:

1. `RangeIter` is tree-traversal based.
   Any wording that says it follows `next_leaf` pointers must be replaced.

2. `next_leaf` is on-disk metadata, not the iterator contract.
   The deferred Phase 7 gap stays visible.

3. Fixed bugs must not remain open tracker items.
   They can remain documented, but only as resolved context.

4. Historical Phase 2 spec/plan must describe the implementation that exists now,
   not the abandoned design sketches (`node.rs`, 12-byte RID layout, `ORDER_LEAF = 211`).

## Implementation phases

1. Update tracker and phase-summary docs:
   - `docs/progreso.md`
   - `docs/fase-2.md`
   - `docs/README.md`

2. Update public-facing Phase 2 summaries:
   - `crates/axiomdb-index/src/lib.rs`
   - `docs-site/src/development/benchmarks.md`

3. Reconcile historical implementation docs:
   - `specs/fase-02/spec-btree.md`
   - `specs/fase-02/plan-btree.md`

4. Run a stale-text sweep with `rg` for:
   - `leaf linked list`
   - `ORDER_LEAF = 211`
   - `node.rs`
   - `RangeIter follows next_leaf`
   - broken `fase-02.md` links in `docs/`

5. Run a narrow safety check:
   - `cargo test -p axiomdb-index --lib`

## Tests to write

- unit:
  - none; this subphase does not change runtime behavior
- integration:
  - none new; reuse the existing `axiomdb-index` test suite as a no-regression smoke check
- bench:
  - none new; benchmark numbers stay unchanged in this subphase
- validation:
  - `rg` sweep confirms the stale phrases were removed from the targeted files
  - `cargo test -p axiomdb-index --lib` still passes

## Anti-patterns to avoid

- Do **not** mark the `next_leaf` CoW gap as fixed just because the iterator already works without it.
- Do **not** “fix” stale docs by deleting historical context about the resolved bugs.
- Do **not** leave `spec-btree.md` and `plan-btree.md` describing files or constants that no longer exist.
- Do **not** change benchmark numbers unless new measurements were actually run.
- Do **not** turn this subphase into a functional B+ Tree refactor.

## Risks

- Historical docs may mix “original intent” with “as-built behavior”.
  Mitigation: rewrite them explicitly toward the current implementation and keep
  deferred/fixed items labeled as such.

- One stale reference may remain outside the obvious Phase 2 files.
  Mitigation: finish with the targeted `rg` sweep defined above.

- Tracker cleanup could accidentally hide a real pending item.
  Mitigation: preserve the `next_leaf` Phase 7 deferment explicitly in both
  `docs/progreso.md` and `docs/fase-2.md`.
