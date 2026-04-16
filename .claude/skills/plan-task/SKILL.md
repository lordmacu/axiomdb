---
name: plan-task
description: Write an implementation plan into specs/fase-N/plan-nombre.md — bite-sized steps, exact files, TDD order, verification commands
---

# /plan-task — Write the implementation plan for a spec

Translate an approved spec into a concrete, executable plan.
The plan is the "how" — ordered steps small enough that each one
compiles, tests, and commits cleanly.

## Usage

```
/plan-task fase-N name
```

Example: `/plan-task 2 btree-insert` writes `specs/fase-02/plan-btree-insert.md`.

## Prerequisites

- [ ] `specs/fase-N/spec-<nombre>.md` exists and is marked `approved`
- [ ] Spec open questions are all resolved
- [ ] Done criteria are clear and verifiable

If the spec is still `draft`, go back to `/spec-task` first.

## Step 1 — Read the spec carefully

```bash
cat specs/fase-N/spec-<nombre>.md
```

Identify:

- Public API to build
- Data structures required
- Edge cases that need tests
- Dependencies on other crates / modules
- Performance budget to verify

## Step 2 — Decompose into tasks

Rules for task size:

- Each task produces a **working commit** (compiles + tests pass for touched scope)
- Each task touches **one crate or one clearly bounded area**
- Each task is small enough to review in **< 15 minutes**
- Tasks follow **TDD order**: test first, implementation second

If a task feels too big, split it. If a task feels trivial (< 2 files, < 20 lines),
merge it with the next one.

## Step 3 — Use this template

```markdown
# Plan: [task name]

Phase: N — [phase name]
Task: [task name within the sprint]
Spec: specs/fase-N/spec-<nombre>.md
Status: draft | in-progress | done

## Summary

[One paragraph. What this plan accomplishes, in what order, and why
this order. Reference the spec for behavior details.]

## Dependencies

Must be done first:
- [ ] spec-[other] approved
- [ ] plan-[other] completed

Blocks (until this plan is done):
- [ ] [name of blocked work]

## Affected files

New files:
- `crates/axiomdb-X/src/module.rs` — [purpose]
- `crates/axiomdb-X/tests/integration.rs` — [purpose]

Modified files:
- `crates/axiomdb-X/src/lib.rs` — add public re-exports
- `crates/axiomdb-X/Cargo.toml` — add dependency [Y]

## Step 1 — [name]

**Goal:** [one line]
**Files:** [list]
**Approach:** TDD — write the failing test first, then the minimal implementation.

### Test to add

\`\`\`rust
// crates/axiomdb-X/tests/module_test.rs
#[test]
fn test_name_describes_behavior() {
    // Arrange
    let x = ...;

    // Act
    let result = x.operation();

    // Assert
    assert_eq!(result, expected);
}
\`\`\`

### Implementation outline

\`\`\`rust
// crates/axiomdb-X/src/module.rs
pub struct Thing { ... }

impl Thing {
    pub fn operation(&self) -> Result<Output, DbError> {
        // 1. Validate input
        // 2. Do the thing
        // 3. Return Ok(output)
        todo!()
    }
}
\`\`\`

### Verification

\`\`\`bash
cargo test -p axiomdb-X --test module_test
cargo clippy -p axiomdb-X -- -D warnings
\`\`\`

### Commit

\`\`\`
feat(fase-N): [concise action]

Step 1 of specs/fase-N/plan-<nombre>.md
\`\`\`

---

## Step 2 — [name]

[Same structure as Step 1.]

---

## Step N — Wire together / final integration

**Goal:** expose public API and ensure the full task spec is met.

### Verification against spec

Walk through every item in the spec's "Done criteria" and check:

- [ ] Public API matches spec signatures
- [ ] Every edge case from spec has a test
- [ ] cargo test -p axiomdb-CRATE passes
- [ ] cargo clippy -p axiomdb-CRATE -- -D warnings passes
- [ ] Benchmarks within budget: [list]

### Final commit

\`\`\`
feat(fase-N): complete [task name]

Implements specs/fase-N/spec-<nombre>.md
Plan: specs/fase-N/plan-<nombre>.md
Tests: [N new tests]
\`\`\`

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| [unknown behavior of dep X] | medium | prototype in Step 1, fail fast |
| [perf may not hit budget] | low | benchmark in Step N-1 before integration |

## Rollback plan

If the plan is abandoned mid-way:

1. `git reset --hard <commit before Step 1>` — or
2. Leave partial work on a branch named `abandoned/plan-<nombre>-<date>`
3. Update spec status back to `draft` with a note explaining what failed

## Estimated effort

Total: [X hours / Y days]
Per step: [step 1: 30min, step 2: 1h, ...]
```

## Step 4 — Review with the user

Before executing the plan:

- [ ] User has read the plan
- [ ] Steps are small enough
- [ ] TDD order makes sense
- [ ] Risks are acceptable
- [ ] Status changed from `draft` to `in-progress`

## Step 5 — Execute and track

- Execute one step at a time
- Commit at each step boundary
- If a step reveals the plan is wrong: STOP, revise the plan,
  commit the plan change, continue
- When all steps are done: mark status `done` in the plan file

## Step 6 — Close

When the plan is complete:

```bash
git add specs/fase-N/plan-<nombre>.md
git commit -m "plan(fase-N): mark plan-<nombre> as done"
```

Then run `/subfase-completa N.M` to close the subphase officially.

## Rules

1. **TDD per step.** Every step starts with a failing test.
2. **Compilable commits.** Each step's commit must build and test-pass.
3. **No speculative scope.** If it's not in the spec, it's not in the plan.
4. **Small steps beat clever steps.** 8 easy steps > 3 elegant mega-steps.
5. **Update the plan when reality diverges.** The plan is a living document
   until status = done.
6. **One spec → one plan.** If a spec needs two plans, split the spec first.
