# AxiomDB — Database Engine in Rust

## Source of truth
`db-summary.md` — compact orientation (read this first, ~100 lines).
`db.md` — full design reference (10K lines); grep specific sections as needed, never read whole file.

---

## Decision-making style

Make decisions autonomously. Only ask when there is a genuine blocker that cannot be resolved
without user input. One question max per session, and only if truly necessary.

---

## Mandatory engineering workflow

```
/brainstorm → /spec-task → /plan-task → /implement-task → /review-task
```

Never skip steps. The full workflow applies **per subphase** (3.1, 3.2…), not per entire phase.
Each subphase has its own spec, plan, implementation, and review before moving to the next.

---

## Effort levels

| Level | When to use |
|---|---|
| `max` | Novel algorithm, safety-critical (crash recovery, MVCC, concurrency), unsafe code, major downstream impact |
| `high` | Complex feature with non-obvious edge cases, significant data structure design, multi-component integration |
| `medium` | Standard implementation of a fully-defined spec, incremental features |
| `low` | Mechanical work: tests, docs, minor additions, format changes |

At the **start of /brainstorm**: recommend effort for each phase (Brainstorm+Spec / Plan / Implementation / Review).
At the **end of /spec-task**: tell the user which effort to use for Plan.

---

## /brainstorm

1. Read `db-summary.md` for orientation; grep `db.md` for the current phase detail
2. Read relevant codebase files
3. Ask clarifying questions BEFORE proposing
4. Propose 2-3 approaches with trade-offs
5. Write sprint with dependencies if subtasks exist

---

## /spec-task

Save in `specs/fase-N/spec-nombre.md`:

```markdown
# Spec: [name]

## What to build (not how)
## Inputs / Outputs
## Use cases
## Acceptance criteria
- [ ] [verifiable criterion]
## Out of scope
## Dependencies
```

---

## /plan-task

Save in `specs/fase-N/plan-nombre.md`:

```markdown
# Plan: [name]

## Files to create/modify
## Algorithm / Data structure
## Implementation phases
## Tests to write
## Anti-patterns to avoid
## Risks
```

---

## /implement-task

### ⚡ MANDATORY: Declare effort BEFORE writing any code

```
⚡ Effort required: [medium | high | max]
Reason: [one line]
```

Then STOP and wait for user confirmation before writing a single line of code.

### Validation during implementation

1. Run tests for the crate you touched: `cargo test -p axiomdb-...`
2. Add directly affected dependents when the change crosses crate boundaries
3. Run `tools/wire-test.py` only if the change is observable through the MySQL wire protocol
4. Run `cargo test --workspace` only when **closing** the subphase/phase

Expand beyond the touched crate when: public API/trait changed, on-disk format changed,
SQL semantics changed, or shared crates (`axiomdb-core`, `axiomdb-types`, `axiomdb-storage`) changed.

### Closing protocol — mandatory on every subphase

```
1.  cargo test --workspace — clean
2.  cargo clippy --workspace -- -D warnings — clean
3.  cargo fmt --check — clean
4.  Wire smoke test (if MySQL-visible): overwrite tools/wire-test.py with new + regression assertions
5.  Write docs/fase-N.md
6.  Update docs/progreso.md — mark subphase [x] ✅, parent phase 🔄
7.  Update docs-site/ — all pages affected by this subphase
8.  Update memory/project_state.md
9.  Update memory/architecture.md
10. Update memory/lessons.md if there were learnings
11. Commit (Conventional Commits format)
12. Push: git push origin main
13. Report progress percentages
14. Confirm to user
```

### Progress report format

```
✅ Subfase X.Y cerrada

📊 Phase N — [████████░░░░░░░░] X/Y subfases (Z.Z%)
🌍 Global  — [██░░░░░░░░░░░░░░] A/B subfases (C.C%)
```

Bar: 1 █ per 6.25% completed, 16 chars total.

### Documentation protocol

Update `docs-site/` on every subphase close. Two audiences:

| Type | Location | Standard |
|---|---|---|
| User docs | `docs-site/src/user-guide/` | Working examples, error codes + fix hints |
| Technical docs | `docs-site/src/internals/` | Algorithms, data layouts, invariants, Rust API examples |

Which pages to update:

| Component changed | User doc | Technical doc |
|---|---|---|
| SQL syntax (parser) | `sql-reference/ddl.md` or `dml.md` | `internals/sql-parser.md` |
| Expression evaluator | `sql-reference/expressions.md` | `internals/sql-parser.md` |
| Executor | `user-guide/getting-started.md`, `sql-reference/dml.md` | `internals/architecture.md` |
| Storage / page format | — | `internals/storage.md` |
| B+ Tree | `user-guide/features/indexes.md` | `internals/btree.md` |
| WAL / crash recovery | `user-guide/features/transactions.md` | `internals/wal.md` |
| MVCC / TxnManager | `user-guide/features/transactions.md` | `internals/mvcc.md` |
| Catalog | `user-guide/features/catalog.md` | `internals/catalog.md` |
| New error type | `user-guide/errors.md` | — |
| Benchmark result | `user-guide/performance.md` | `development/benchmarks.md` |
| New phase completed | `development/roadmap.md` | All internals pages for that phase |

**Callouts:** Add `callout-advantage` / `callout-design` / `callout-tip` divs whenever:
outperforming a named system, avoiding a known cost, borrowing a technique, deriving a constant,
or a benchmark is ≥2× better. Name the competitor explicitly + quantify. See existing docs for HTML template.

### Anti-gap rule

If something cannot be fully implemented, mark it explicitly:
- In the spec: `## ⚠️ DEFERRED — [description] → pending in subphase X.Y`
- In `docs/progreso.md`: `- [ ] ⚠️ [short description] — gap identified, revisit in [subphase]`

### Maximum implementation principle

Always implement the most complete and correct version possible. Only mark DEFERRED when there
is a real dependency on another phase or a documented external limitation — never due to complexity.

### Commit format

```
feat(fase-N): concise description

- detail 1
- detail 2

Phase N/34 completed. See docs/fase-N.md
Spec: specs/fase-N/ | Tests: crates/axiomdb-X/tests/
```

Do not include Co-Authored-By from Claude.

### GitHub account

Use the **lordmacu** account: `user.name = lordmacu`, `user.email = lordmacu@users.noreply.github.com`.
Verify: `gh auth status` must show `github.com` active account `lordmacu`.

### Git branches

```
main              → stable code
fase-N-nombre     → phase development
hotfix/nombre     → urgent fixes on top of main
```

---

## /review-task

### Step 1 — Explore subagent review

Launch an Explore agent to check:
1. Each acceptance criterion from all specs — ✅/❌
2. `unwrap()` in production `src/` (excluding `#[cfg(test)]`) → blocker
3. `unsafe` without `SAFETY:` comment → blocker
4. Integration tests in `tests/` → blocker if missing
5. Benchmarks in `benches/` compile → blocker if not
6. Test logic — assertions correct? Tests that always pass without verifying anything?
7. Unidentified gaps vs spec
8. Documentation staleness — `docs-site/` pages updated for all components touched? → blocker
9. Callouts present for every mandatory trigger (outperform, avoid cost, borrow technique, ≥2× benchmark) → blocker

### Step 2 — Fix all blockers

### Step 3 — Mandatory benchmarks

```bash
cargo bench --bench [nombre] 2>&1 | tee /tmp/bench-fase-N.txt
```

Report table: Benchmark | AxiomDB | MySQL (aprox) | PostgreSQL (aprox) | Target | Máx aceptable | Veredicto

Thresholds: ✅ beats MySQL or PostgreSQL / ⚠️ within max / ❌ below max (blocker)

### Step 4 — Closing checklist

```
[ ] All acceptance criteria ✅
[ ] cargo test --workspace ✅
[ ] cargo clippy -- -D warnings ✅
[ ] cargo fmt --check ✅
[ ] No unwrap() in src/ ✅
[ ] All unsafe has SAFETY: comment ✅
[ ] Integration tests in tests/ ✅
[ ] Wire smoke test updated and passing ✅
[ ] Benchmarks run and reported ✅
[ ] No benchmark ❌ ✅
[ ] docs/progreso.md updated ✅
[ ] docs-site/src/internals/ updated ✅
[ ] docs-site/src/user-guide/ updated ✅
[ ] development/roadmap.md updated ✅
[ ] Callouts added for every mandatory trigger ✅
[ ] Commit done ✅
[ ] git push origin main ✅
```

---

## /debug

1. Reproduce with the minimal failing test
2. Formulate at least 2 hypotheses
3. Design an experiment for each
4. Fix in the right place — not the symptom
5. Add regression test

---

## /bench

```bash
cargo bench --bench [nombre] > /tmp/before.txt
# make change
cargo bench --bench [nombre] > /tmp/after.txt
cargo install critcmp && critcmp /tmp/before.txt /tmp/after.txt
```

Regression > 5% on a critical operation: blocker.

### Performance budget

| Operation | AxiomDB actual | MySQL (aprox) | PostgreSQL (aprox) | Target | Máx aceptable |
|---|---|---|---|---|---|
| Point lookup PK | TBD | ~830K ops/s | ~1.1M ops/s | 800K ops/s | 600K ops/s |
| Range scan 10K rows | 0.61ms ✅ | ~8ms | ~5ms | 45ms | 60ms |
| INSERT with WAL | TBD | ~150K ops/s | ~120K ops/s | 180K ops/s | 150K ops/s |
| Seq scan 1M rows | TBD | ~0.8s | ~0.5s | 0.8s | 1.2s |
| Parser simple SELECT | 500ns ✅ | ~500ns | ~450ns | — | — |
| Parser complex SELECT | 2.7µs ✅ | ~4µs | ~3.5µs | — | — |
| Row codec encode | 30ns ✅ | N/A | N/A | — | — |
| Expr eval scan/1K | 14.8M/s ✅ | ~8M rows/s | ~6M rows/s | — | — |

---

## /unsafe-review

For each `unsafe` block: document with `// SAFETY: [invariant]`, verify a safe alternative
doesn't exist, confirm there's a test for the contract, and encapsulate in a public safe function.

---

## /new-crate

`cargo new --lib crates/axiomdb-X`, add to workspace `Cargo.toml`, define public types in `lib.rs`,
write initial compiling test, update `memory/architecture.md`.

---

## /profile

`cargo flamegraph --bench [nombre]` or `cargo samply record cargo bench --bench [nombre]`.
Identify the hottest function, optimize only that, verify improvement, verify no regression.

---

## /fuzz

`cargo fuzz add fuzz_target && cargo fuzz run fuzz_target -- -max_total_time=300`.
For each crash: add regression test in `tests/`, fix bug, verify test passes.

---

## /checkpoint

Create `docs/checkpoint-YYYYMMDD.md` with: what was being done, pending decision, exact next step,
modified files, test state. Commit it. Next session: read it first.

---

## /subfase-completa

Mark a subphase as completed. Full protocol in `.claude/skills/subfase-completa.md`.
Usage: `/subfase-completa N.M` (e.g., `/subfase-completa 3.2`).

---

## /fase-completa

Full phase close protocol. Runs final tests, benchmarks, documents, updates memory, commits.
Full protocol in `.claude/skills/fase-completa.md`.
Usage: `/fase-completa N` (e.g., `/fase-completa 3`).

---

## /scan-gaps (PROACTIVE)

Discover undocumented gaps by comparing AxiomDB against MySQL/PostgreSQL behavior.
Full protocol in `.claude/skills/scan-gaps.md`. Flow:

1. Build feature inventory from progreso.md + specs + code
2. Create checklist comparing against MySQL/PostgreSQL expectations
3. Create `tools/tmp_gap_test.py` to verify each feature
4. Classify: WORKS / NEW GAP / KNOWN GAP / DEFERRED / EDGE CASE
5. Document new gaps in progreso.md
6. Hand off each new gap to `/hunt-gap` for fixing

Usage: `/scan-gaps fase 3` | `/scan-gaps area parser` | `/scan-gaps fase 1-11` | `/scan-gaps full`

### Proactive triggers — use `/scan-gaps` automatically when:

- The user says "busca gaps", "find bugs", "scan for issues", "compare with MySQL"
- The user asks to audit or review a phase for completeness
- After closing a major phase, to verify nothing was missed

---

## /hunt-gap (PROACTIVE)

Proactive gap/bug hunter. Verify and fix documented gaps one at a time.
Full protocol in `.claude/skills/hunt-gap.md`. Flow:

1. Locate gap in `docs/progreso.md` + `specs/`
2. Reproduce: create `tools/tmp_gap_test.py` (Python, connects via MySQL wire) or grep for code patterns
3. Classify: CONFIRMED / ALREADY-FIXED / PARTIALLY-FIXED
4. Root cause: 2+ hypotheses, investigate, identify exact file:line
5. Fix: minimal change, build + test incrementally
6. Validate: wire-test regression check, clippy, delete tmp file, update progreso.md

Usage: `/hunt-gap GAP-B.1` | `/hunt-gap fase 3` | `/hunt-gap fase 1-11` | `/hunt-gap all`

### Proactive triggers — use `/hunt-gap` automatically when:

- The user mentions **gaps**, **bugs**, **unwrap**, **panics**, or **missing features**
- The user asks to **review**, **audit**, or **check** code for problems
- During `/review-task`, if acceptance criteria reveal a gap not in progreso.md
- During `/implement-task`, if a test failure reveals an undocumented bug
- When reading `docs/progreso.md` and seeing `⏳` items in phases already closed
- When the user says "fix", "solve", "hunt", "find bugs", "verify gaps"
- After completing a subphase close, if collateral findings were documented

When triggered proactively, announce it:
```
Detected [trigger reason]. Activating /hunt-gap protocol for [gap-id].
```
Then follow the full protocol from `.claude/skills/hunt-gap.md`.

---

## Rust code conventions

- **Errors**: use `thiserror`. No `unwrap()` in `src/` — use `?` or `map_err(...)`.
- **Async**: I/O → tokio async. CPU-intensive → rayon + `spawn_blocking`. Never block tokio runtime.
- **Unsafe**: always requires `// SAFETY: [invariant]` comment. No exceptions.
- **Testing**: unit tests use `MemoryStorage` (no I/O). Integration tests use real I/O + `tempfile`.
  Benchmarks measure only — no assertions.

---

## Project structure

```
axiomdb/
├── CLAUDE.md              ← workflow
├── db.md                  ← complete design (source of truth)
├── Cargo.toml             ← workspace root
├── specs/fase-N/          ← specs and plans
├── docs/                  ← phase docs + progreso.md
├── crates/                ← source code
├── tests/                 ← integration tests
├── benches/               ← criterion benchmarks
└── fuzz/                  ← cargo-fuzz targets
```

---

## Memory protocol

Files in `memory/` (project) and `/root/.claude/projects/-home-familia-axiomdb/memory/` (auto-memory):

| File | When to update |
|---|---|
| `memory/project_state.md` | Always when closing a phase |
| `memory/architecture.md` | When a crate is created or modified |
| `memory/lessons.md` | When something surprising happens |

---

## Before each session

```
1. Read docs/checkpoint-*.md if it exists
2. Read docs/fase-N.md from the last completed phase
3. Read specs/fase-actual/
4. Continue from where it was left off
```
