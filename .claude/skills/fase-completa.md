# /fase-completa — Phase close protocol

Run this COMPLETE protocol when finishing each phase. No exceptions.

## Step 1 — Verify code quality

```bash
# Tests
cargo test --workspace
# If it fails: DO NOT continue until they pass

# Linting
cargo clippy --workspace -- -D warnings
# If there are warnings: DO NOT continue until resolved

# Format
cargo fmt --check
# If there are differences: run cargo fmt and add to the commit

# Unsafe documentation
grep -r "unsafe" crates/ --include="*.rs" | grep -v "// SAFETY:"
# If there are unsafe blocks without SAFETY: add the comment
```

## Step 2 — Benchmarks (if applicable)

```bash
cargo bench --workspace 2>&1 | tail -30
# Compare against budget in CLAUDE.md
# If any critical operation regressed >5%: investigate before continuing
```

## Step 3 — Document the phase

Create `docs/fase-N.md` with this template:

```markdown
# Phase N — [Name]

Completed: [date]
Estimated weeks: N-M | Actual weeks: X

## What was built
[description of what was implemented]

## Crates created/modified
- `crates/dbyo-X` — [what it does]

## Decisions made
- [decision] → [reason]

## Tests written
- [test] — [what it verifies]

## How to continue from here
[exact instruction for the next session]

## Next phase
Phase N+1 — [name]. See `specs/fase-N+1/` and `db.md`.
```

## Step 4 — Update memory

```
memory/project_state.md:
  - Move phase N from "pending" to "completed"
  - Update "Current phase" to N+1

memory/architecture.md:
  - Add the actually implemented crates and structs
  - Update the directory tree

memory/decisions.md:
  - Add technical decisions made during the phase

memory/lessons.md (if there were learnings):
  - ### [Phase N] Learning title
  - Problem, cause, solution, when to apply
```

## Step 5 — Commit

```bash
git add -A
git commit -m "feat(fase-N): [concise description]

- [detail 1]
- [detail 2]

Phase N/34 completed. See docs/fase-N.md
Spec: specs/fase-N/ | Tests: X passing"
```

## Step 6 — Confirm to user

Report:
```
✅ Phase N completed

Tests:      X/X passing
Clippy:     0 warnings
Benchmarks: [within/outside] budget

Documented in: docs/fase-N.md
Memory:        updated
Commit:        [hash]

Next phase: N+1 — [name]
To continue: /brainstorm on Phase N+1
```
