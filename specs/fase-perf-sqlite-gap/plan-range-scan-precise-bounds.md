# Plan: range-scan-precise-bounds

Phase: perf-sqlite-gap — close embedded read gap with SQLite
Task: precise inclusive/exclusive range bounds + skip per-row WHERE re-eval for
covering clustered-PK ranges
Spec: specs/fase-perf-sqlite-gap/spec-range-scan-precise-bounds.md
Status: done

## Summary

Five steps. Step 1 is a pure, behavior-preserving refactor: add three fields to
`AccessMethod::IndexRange` (`lo_inclusive`, `hi_inclusive`, `covers_predicate`)
and update every construction (conservative defaults `true/true/false`) and
destructure site so the workspace still compiles and all tests pass unchanged.
Step 2 teaches the planner to report exact bound strictness and set
`covers_predicate=true` for the two SELECT pure-range rules (Rule 2 two-sided,
Rule 2b single-sided). Step 3 makes `range_clustered_table` honor strictness via
`Bound` (`range_callback` already supports it). Step 4 wires the clustered-PK
`IndexRange` executor arm to pass strictness and set
`where_already_applied = covers_predicate`, eliminating the per-row WHERE re-eval.
Step 5 verifies workspace + wire + bench. Order is bottom-up so each commit is
green: the enum churn lands first (no behavior change), then the planner signal,
then the scan, then the executor flips the optimization on.

## Dependencies

Must be done first:
- [x] spec-range-scan-precise-bounds approved

Blocks (until done):
- [ ] extending precise bounds to secondary / composite / heap ranges (follow-up)

## Affected files

Modified:
- `crates/axiomdb-sql/src/planner_types.rs` — 3 new `IndexRange` fields
- `crates/axiomdb-sql/src/planner_select.rs` — `extract_range`/`extract_range_side`
  return strictness; Rule 2 + Rule 2b set precise bounds + `covers_predicate=true`
- `crates/axiomdb-sql/src/planner_ctx.rs` — DELETE/UPDATE range sites: add
  conservative default fields (no behavior change)
- `crates/axiomdb-sql/src/table.rs` — `range_clustered_table` honors strictness
- `crates/axiomdb-sql/src/executor/select_ctx.rs` — clustered-PK `IndexRange` arm:
  pass strictness + `where_already_applied = covers_predicate`
- Destructure sites (compile-only `..`): `select_core.rs`, `exec_explain.rs`,
  `update_ctx.rs`, `delete.rs`, `update_fused_range.rs`, `agg_hash.rs` (only those
  that bind `lo`/`hi`)
- `tools/wire-test.py` — exclusive-bound range correctness assertion (Step 5)

No new files. No on-disk format change.

---

## Step 1 — Add IndexRange fields (pure refactor, no behavior change)

**Goal:** `IndexRange` carries `lo_inclusive`/`hi_inclusive`/`covers_predicate`;
everything compiles and behaves exactly as before.
**Files:** `planner_types.rs` + every IndexRange construction/destructure site.
**Approach:** mechanical. Defaults preserve today's semantics (inclusive bounds,
re-eval on).

### Implementation outline

```rust
// planner_types.rs
IndexRange {
    index_def: IndexDef,
    lo: Option<Vec<u8>>,
    hi: Option<Vec<u8>>,
    lo_inclusive: bool,   // NEW
    hi_inclusive: bool,   // NEW
    covers_predicate: bool, // NEW
}
```

Every construction (planner_select 90/110/141/173/578; planner_ctx
131/154/171/199/219/234) adds:
```rust
lo_inclusive: true, hi_inclusive: true, covers_predicate: false,
```
Every destructure that binds `lo`/`hi` but not the new fields adds `, ..`.
`matches!(.., IndexRange { .. })` and `{ index_def, .. }` sites are unchanged.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql        # all pass — pure refactor
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```
No new test (a no-op refactor is proven by the existing suite staying green).

### Commit

```
refactor(perf-sqlite-gap): add strictness+covering fields to IndexRange

Step 1 of specs/fase-perf-sqlite-gap/plan-range-scan-precise-bounds.md
```

---

## Step 2 — Planner reports strictness + covering for SELECT pure ranges

**Goal:** Rule 2 (`col >= lo AND col < hi`) and Rule 2b (single-sided) emit exact
`lo_inclusive`/`hi_inclusive` and `covers_predicate=true`.
**Files:** `planner_select.rs`.
**Approach:** TDD — planner unit tests assert the new fields, then implement.

### Test to add

```rust
// in planner_select.rs #[cfg(test)] (mirror existing planner tests)
#[test]
fn two_sided_range_is_covering_with_exact_strictness() {
    // WHERE id >= 10 AND id < 20  (PK `id`)
    let am = plan_for("id >= 10 AND id < 20", /* pk index */);
    match am {
        AccessMethod::IndexRange { lo_inclusive, hi_inclusive, covers_predicate, .. } => {
            assert!(lo_inclusive);        // >=
            assert!(!hi_inclusive);       // <
            assert!(covers_predicate);    // whole WHERE is the range
        }
        other => panic!("expected IndexRange, got {other:?}"),
    }
}

#[test]
fn single_sided_gt_is_exclusive_lower_and_covering() {
    let am = plan_for("id > 50", /* pk */);
    match am {
        AccessMethod::IndexRange { lo_inclusive, covers_predicate, hi, .. } => {
            assert!(!lo_inclusive);       // >
            assert!(hi.is_none());        // open upper
            assert!(covers_predicate);
        }
        other => panic!("got {other:?}"),
    }
}
```

### Implementation outline

```rust
// extract_range_side: also return inclusivity
fn extract_range_side(expr: &Expr) -> Option<(&str, Option<Value>, bool /*inclusive*/)> {
    // GtEq/LtEq -> inclusive=true ; Gt/Lt -> inclusive=false
}

// extract_range: thread per-side inclusivity out
fn extract_range(..) -> Option<(&IndexDef, (Option<Value>, bool), (Option<Value>, bool))>;

// Rule 2 (planner_select.rs ~141): set fields from the two sides
AccessMethod::IndexRange { index_def, lo, hi, lo_inclusive, hi_inclusive, covers_predicate: true }

// Rule 2b (~173): is_lower decides which side; the operator decides inclusivity
//   id >= v -> lo_inclusive=true ; id > v -> lo_inclusive=false ; (hi unbounded => hi_inclusive irrelevant, keep true)
AccessMethod::IndexRange { .., covers_predicate: true }
```
Leave Rule 1 (composite-eq expansion), Rule 1b (expression index), Rule 578
(composite range) at `covers_predicate=false` (out of scope — they may have
residual / padded bounds).

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql planner   # new + existing planner tests
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(perf-sqlite-gap): planner emits exact range strictness + covering

Step 2 of specs/fase-perf-sqlite-gap/plan-range-scan-precise-bounds.md
```

---

## Step 3 — range_clustered_table honors strictness

**Goal:** the clustered range scan excludes an exclusive boundary key.
**Files:** `table.rs` (+ minimal caller update to keep compiling).
**Approach:** TDD — integration test on exclusive upper bound, then implement.

### Test to add

```rust
// crates/axiomdb-sql/tests/integration_clustered_range.rs (or existing range test file)
#[test]
fn clustered_range_excludes_exclusive_upper() {
    // insert id = 1..=10 into a clustered PK table
    // range_clustered_table(lo=Included(3), hi=Excluded(7)) => ids {3,4,5,6}
    let rows = range_clustered_table(.., Some(&enc(3)), Some(&enc(7)),
                                     /*lo_inclusive=*/true, /*hi_inclusive=*/false, snap)?;
    assert_eq!(ids(rows), vec![3,4,5,6]);   // 7 excluded
}
```

### Implementation outline

```rust
pub fn range_clustered_table(
    .., lo: Option<&[u8]>, hi: Option<&[u8]>,
    lo_inclusive: bool, hi_inclusive: bool, snap,
) -> ... {
    let from = match lo {
        Some(k) if lo_inclusive => Bound::Included(k.to_vec()),
        Some(k) => Bound::Excluded(k.to_vec()),
        None => Bound::Unbounded,
    };
    let to = match hi {
        Some(k) if hi_inclusive => Bound::Included(k.to_vec()),
        Some(k) => Bound::Excluded(k.to_vec()),
        None => Bound::Unbounded,
    };
    // range_callback already handles Excluded
}
```
Update the `select_ctx.rs` clustered-PK call to pass `true, true` for now (Step 4
flips to real values) — keeps this commit behavior-preserving. Grep for any other
`range_clustered_table` callers and pass `true, true`.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql clustered_range
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(perf-sqlite-gap): range_clustered_table honors exclusive bounds

Step 3 of specs/fase-perf-sqlite-gap/plan-range-scan-precise-bounds.md
```

---

## Step 4 — Executor: pass strictness + skip re-eval when covering

**Goal:** clustered-PK `IndexRange` arm uses precise bounds and skips the per-row
WHERE re-eval when `covers_predicate`.
**Files:** `select_ctx.rs`.
**Approach:** TDD — integration test for exactness + residual-still-filters.

### Test to add

```rust
// crates/axiomdb-sql/tests/integration_select.rs
#[test]
fn pk_range_exclusive_upper_returns_exact_rows() {
    // SELECT * FROM t WHERE id >= 3 AND id < 7  => ids {3,4,5,6}, 7 excluded
}
#[test]
fn pk_range_with_residual_still_filters() {
    // SELECT * FROM t WHERE id >= 3 AND id < 9 AND active = TRUE
    // range covers id; active=TRUE residual MUST still filter
}
```

### Implementation outline

```rust
// select_ctx.rs clustered-PK IndexRange arm (~710)
AccessMethod::IndexRange { index_def, lo, hi, lo_inclusive, hi_inclusive, covers_predicate }
    if resolved.def.is_clustered() && index_def.is_primary =>
{
    if *covers_predicate { where_already_applied = true; }
    crate::table::range_clustered_table(
        storage, &resolved.def, &resolved.columns,
        lo.as_deref(), hi.as_deref(), *lo_inclusive, *hi_inclusive, snap,
    )?
}
```
Other IndexRange arms (clustered-secondary ~724, heap ~736) keep current behavior
(do NOT read `covers_predicate`; they don't apply precise bounds, so they must
keep re-eval — safe). `pk_range_with_residual` is covered because Rule 2/2b only
set `covers_predicate=true` when the WHERE is exactly the range; a residual makes
`extract_range` fail → `Scan`/other method → re-eval stays on.

### Verification

```bash
./tools/vm.sh test -p axiomdb-sql
./tools/vm.sh clippy -p axiomdb-sql -- -D warnings
```

### Commit

```
feat(perf-sqlite-gap): skip per-row WHERE re-eval for covering PK ranges

Step 4 of specs/fase-perf-sqlite-gap/plan-range-scan-precise-bounds.md
```

---

## Step 5 — Workspace + wire + bench

**Goal:** no regressions; range_scan within budget.
**Files:** `tools/wire-test.py` (+ docs-site note).

### Verification

```bash
./tools/vm.sh test --workspace            # run ALONE
./tools/vm.sh clippy --workspace -- -D warnings
./tools/vm.sh fmt-check
# wire (pre-flight: pkill + rebuild server)
pkill -f axiomdb-server; ./tools/vm.sh wire   # add: range with exclusive upper returns exact rows
# correctness vs SQLite
cargo build -p axiomdb-server --release && python3 tools/verify-select-where.py   # if it covers ranges
# bench on macOS native (the representative env for scans)
cargo build --release -p axiomdb-bench-comparison
for i in 1 2 3 4 5; do ./target/release/axiomdb_bench --compare --rows 100000 | grep -E "range_scan|full_scan|point_lookup"; done
```

### Done-criteria check (from spec)
- [ ] IndexRange carries the 3 fields; producers conservative except Rule 2/2b
- [ ] range_clustered_table honors strictness
- [ ] clustered-PK arm: precise bounds + where_already_applied=covers_predicate
- [ ] edge cases tested (exclusivity, residual, MVCC, empty range)
- [ ] workspace test/clippy/fmt clean; wire exact-rows assertion passes
- [ ] bench: range_scan ≤1.1× (10K range, macOS); full_scan/point_lookup not regressed

### Final commit

```
feat(perf-sqlite-gap): precise range bounds, skip covered re-eval (range_scan)

Implements specs/fase-perf-sqlite-gap/spec-range-scan-precise-bounds.md
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| `covers_predicate=true` set when range doesn't cover WHERE → row leak | low | only Rule 2/2b set it, and they fire only when the WHERE is exactly the range (`extract_range`/`extract_range_side` match the whole expr); residual → no match → re-eval stays. Test `pk_range_with_residual_still_filters` |
| Enum churn breaks an unseen construction site | low | compiler-checked; Step 1 is pure refactor, full suite must stay green |
| Exclusive bound off-by-one in encoding | low | strictness handled at `Bound` level (range_callback), not key encoding; test `clustered_range_excludes_exclusive_upper` |
| Other IndexRange arms read stale `covers_predicate` | low | only the clustered-PK arm reads it; others ignore it + keep re-eval |
| Gain smaller than hoped (fixed overhead dominates small ranges) | medium | bench at `--rows 100000` (10K range) where per-row cost dominates; accept ~1.1× |

## Rollback plan

1. `git reset --hard <commit before Step 1>` — change is contained to axiomdb-sql.
2. Or branch `abandoned/plan-range-scan-precise-bounds-2026-05-20`.
3. Spec status back to `draft` with a failure note.

## Estimated effort

Effort: **high** (planner + executor + bound-correctness across two layers).
Total: ~1 day.
- Step 1: 1–1.5h (mechanical churn) · Step 2: 2h · Step 3: 1h · Step 4: 1.5h · Step 5: 1h
