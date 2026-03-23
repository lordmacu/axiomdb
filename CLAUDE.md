# NexusDB — Database Engine in Rust

## Source of truth
The `db.md` file contains the complete design: architecture, types, phases, crates, and decisions.
**Read it before any task.**

---

## Decision-making style

Make decisions autonomously. Do not ask for confirmation on implementation details,
approach choices, or anything that can be derived from the spec and plan.
Only ask when there is a genuine blocker that cannot be resolved without user input
(e.g., two valid approaches with fundamentally different trade-offs not covered in the spec).
One question max per session, and only if truly necessary.

---

## Effort per phase

- `/brainstorm`, `/spec-task`, `/plan-task`: reason in depth before responding,
  consider multiple approaches, anticipate edge cases. Use `/effort max` for these phases.
- `/implement-task`, `/review-task`: execute directly without excessive deliberation.
  Default effort is sufficient.

**Reminder:** activate `/effort max` before brainstorm/spec/plan, and `/effort` (no arg)
before implementing. It takes 2 seconds and makes a difference in design decisions.

### Effort level guidance — mandatory at two moments

**1. At the START of every /brainstorm:**

Before doing anything else, assess the subphase complexity and tell the user:

```
⚡ Effort recommendation for [subfase X.Y]:
  Brainstorm + Spec:  [level]
  Plan:               [level]
  Implementation:     [level]
  Review:             [level]
```

Use these criteria to determine the level:

| Level | When to use |
|-------|-------------|
| `max` | Novel algorithm design, safety-critical decisions (crash recovery, MVCC, concurrency), choices with major downstream impact on multiple future phases, or any unsafe code |
| `high` | Complex feature with non-obvious edge cases, significant data structure design, integration of multiple components |
| `medium` | Standard implementation of a fully-defined spec, incremental features, adding methods to existing structures |
| `low` | Mechanical work: adding tests, updating docs, minor additions to existing logic, format changes |

**2. At the END of every /spec-task:**

After writing the spec (while still at max effort), tell the user:

```
✅ Spec written. You can now switch to /effort [level] for the Plan phase.
```

This tells the user exactly when to switch — no guessing.

---

## Mandatory engineering workflow

```
/brainstorm → /spec-task → /plan-task → /implement-task → /review-task
```

Never skip steps. Specs before code. Plans before implementation.
Reviews before closing. No exceptions.

### Unit of work: subphase, not phase

The full workflow applies **per subphase** (3.1, 3.2, 3.3…), not per entire phase.
Each subphase has its own spec, plan, implementation, and review before moving to the next.
Never implement multiple subphases together without closing each one individually.

**Why subphases and not full phases:**
Working per subphase ensures each piece receives all the engineering attention it needs.
When working on full phases, it is easy to overlook details that only surface when planning
a single thing in depth — and those overlooked details turn into gaps, bugs,
or costly rework in future phases. A well-closed subphase (spec → plan → code → review)
is a subphase that will never be reopened. The goal is not to move fast on paper, but to have
each component correct, robust, and free of hidden technical debt from day one.

---

## /brainstorm — Explore before proposing

1. Read the `db.md` section for the current phase
2. Read relevant files in the codebase
3. Ask the user questions BEFORE proposing:
   - What is the exact expected behavior?
   - What are the known edge cases?
   - What are the performance constraints?
4. Propose 2-3 approaches with trade-offs (not just the best one)
5. Write a sprint with dependencies if there are subtasks:

```
Sprint: [phase name]
├── Task 1: [description] — no dependencies
├── Task 2: [description] — depends on Task 1
└── Task 3: [description] — depends on Task 1
```

**Output:** sprint + agreed approach.

---

## /spec-task — Requirements before implementing

Save in `specs/fase-N/spec-nombre.md`:

```markdown
# Spec: [name]

## What to build (not how)
[exact behavior]

## Inputs / Outputs
- Input: [exact types]
- Output: [exact types]
- Errors: [when and which]

## Use cases
1. [happy path]
2. [edge case 1]
3. [edge case 2]

## Acceptance criteria
- [ ] [verifiable criterion]

## Out of scope
- [what it does NOT do]

## Dependencies
- [what must exist first]
```

The spec must be self-contained — a new session must understand it without extra context.

---

## /plan-task — Technical plan before coding

Save in `specs/fase-N/plan-nombre.md`:

```markdown
# Plan: [name]

## Files to create/modify
- `crates/dbyo-X/src/Y.rs` — [what it does]

## Algorithm / Data structure
[pseudocode of the approach]

## Implementation phases
1. [concrete verifiable step]
2. [concrete verifiable step]

## Tests to write
- unit: [what to test]
- integration: [what to test end-to-end]
- bench: [what to measure]

## Anti-patterns to avoid
- [DO NOT do X because Y]

## Risks
- [known risk → mitigation]
```

---

## /implement-task — Execute phase by phase

### ⚡ MANDATORY: Declare effort level BEFORE writing any code

Before starting implementation of any subphase, you MUST explicitly state:

```
⚡ Effort required: [medium | high | max]
Reason: [one line — why this level, what makes it hard]
```

Then STOP and wait for the user to confirm (and adjust the model if needed)
before writing a single line of implementation code.

**NEVER skip this declaration. NEVER start coding without user confirmation.**

#### Effort level criteria

| Level | When it applies |
|---|---|
| **medium** | Plumbing over existing infrastructure. Well-known pattern. Low risk of subtle bugs. No critical correctness invariants. |
| **high** | Multiple interacting components. Algorithmic depth. Risk of subtle bugs. Ordering or timing matters. Wrong implementation causes detectable errors. |
| **max** | Critical correctness for data integrity. Formal state machines. Concurrency. A bug = silent data corruption or data loss. Requires deep reasoning about invariants. |

#### Examples by subphase

| Subphase | Level | Reason |
|---|---|---|
| WAL Checkpoint | high | page flush → WAL truncate ordering is critical |
| Crash Recovery | max | replay ordering + partial writes + state machine |
| MVCC full (Phase 7) | max | snapshot isolation, SSI, concurrent access |
| SQL DDL parser | high | grammar non-trivial, type system |
| Vectorized execution | max | SIMD correctness + morsel-driven pipeline |
| WAL Rotation | medium | plumbing over existing checkpoint |
| Simple CRUD executor | high | heap+index integration, MVCC visibility |

---

One phase at a time. When finishing each phase — **mandatory closing protocol:**

```
1. cargo test --workspace passes clean
2. cargo clippy --workspace -- -D warnings with no errors
3. cargo fmt --check with no differences
4. Write docs/fase-N.md
5. Update docs/progreso.md — mark subphase with [x] ✅ and parent phase with 🔄
6. Update docs-site/ — update ALL pages affected by this subphase (see Documentation protocol below)
7. Update memory/project_state.md
8. Update memory/architecture.md
9. Update memory/lessons.md if there were learnings
10. Commit with Conventional Commits format
11. Report progress percentages to the user (see below)
12. Confirm to the user
```

### Progress report — mandatory after every subfase close

After marking the subfase `[x]` in progreso.md, always calculate and report:

```
COMPLETED_GLOBAL=$(grep "^\- \[x\]" docs/progreso.md | wc -l | tr -d ' ')
TOTAL_GLOBAL=$(grep "^\- \[.\]" docs/progreso.md | wc -l | tr -d ' ')
PCT_GLOBAL=$(echo "scale=1; $COMPLETED_GLOBAL * 100 / $TOTAL_GLOBAL" | bc)

# Count current phase subfases from progreso.md (adjust pattern per phase)
PHASE_DONE=<count completed subfases of current phase>
PHASE_TOTAL=<count total subfases of current phase>
PCT_PHASE=$(echo "scale=1; $PHASE_DONE * 100 / $PHASE_TOTAL" | bc)

echo "📊 Phase N:  $PHASE_DONE/$PHASE_TOTAL subfases ($PCT_PHASE%)"
echo "🌍 Global:   $COMPLETED_GLOBAL/$TOTAL_GLOBAL subfases ($PCT_GLOBAL%)"
```

Report format to show the user after every closed subfase:

```
✅ Subfase X.Y cerrada

📊 Phase N — [████████░░░░░░░░] X/Y subfases (Z.Z%)
🌍 Global  — [██░░░░░░░░░░░░░░] A/B subfases (C.C%)
```

Generate the bar with: █ per 6.25% completed, ░ for remaining (16 chars total).

### Documentation protocol — mandatory on every subphase close

The `docs-site/` directory contains two types of documentation that must always be kept in sync with the code:

#### Two audiences — both must be updated

| Type | Location | Who reads it | Standard |
|---|---|---|---|
| **User docs** | `docs-site/src/user-guide/` | Developers using NexusDB as a database | Clear, example-heavy, covers behavior from the outside |
| **Technical docs** | `docs-site/src/internals/` | Contributors, engineers reading the source | Deep, implementation-level, covers algorithms, data structures, invariants |

#### What "explicit and descriptive" means

Every page — user or technical — must meet this bar:

- **User docs:** a developer who has never read the source code must be able to use the feature correctly after reading the page. Every SQL feature needs at least one real working example. Error messages must be shown with their SQLSTATE code and a fix hint.
- **Technical docs:** a contributor who is new to the codebase must be able to understand the algorithm, the on-disk/in-memory layout, the invariants that must hold, and the edge cases. Include:
  - Data structure layout (fields, sizes, bit-level format if relevant)
  - Step-by-step description of every non-trivial algorithm
  - The invariants that the code maintains
  - What happens on error / recovery
  - Code examples (Rust) showing the public API and the critical internal logic
  - Performance characteristics (O(n) guarantees, cache behavior, allocation profile)

#### Callouts — show readers why NexusDB is better

Every doc page that introduces a non-trivial design or implementation must have at
least one callout wherever something noteworthy happened. The goal: a reader skimming
the docs should be able to spot, at a glance, what decisions were made, why we made
them, and how they compare to any relevant database or library.

**Three callout types — use the right one:**

| Type | HTML class | Icon | Use when |
|---|---|---|---|
| **Advantage** | `callout-advantage` | 🚀 | NexusDB measurably outperforms or avoids a cost that another database or library pays |
| **Design Decision** | `callout-design` | ⚙️ | A non-obvious implementation choice was made — a trade-off was evaluated, an approach was borrowed from another system, or a constraint (page size, bit width, scan strategy) was derived deliberately |
| **Tip** | `callout-tip` | 💡 | User-facing: a non-obvious usage pattern, a caveat, or a migration hint |

**HTML syntax (copy-paste template):**

```html
<div class="callout callout-advantage">
<span class="callout-icon">🚀</span>
<div class="callout-body">
<span class="callout-label">Performance Advantage</span>
Explanation here. Be specific: name the competitor, the metric, and the reason.
</div>
</div>
```

Replace `callout-advantage` / `🚀` / `Performance Advantage` with the appropriate
type and label. The label is free-form but should be short (3–5 words).

**Reference systems — compare against the most relevant one for each context:**

| Category | Systems to compare against |
|---|---|
| Relational SQL | MySQL 8 (InnoDB), PostgreSQL 15, SQLite 3 |
| Embedded / serverless | SQLite 3, DuckDB, libsql |
| LSM-tree storage | RocksDB, LevelDB, PebbleDB (CockroachDB's engine) |
| Distributed SQL | CockroachDB, TiDB, YugabyteDB, Spanner |
| In-memory / cache | Redis, DragonflyDB |
| Time-series | InfluxDB, TimescaleDB, ClickHouse |
| Rust libraries | sqlparser-rs, gluesql, sled, fjall |

Always pick the **most relevant** system — not always MySQL/PostgreSQL. If we
implement something that beats RocksDB's compaction overhead, compare with RocksDB.
If we beat DuckDB's vectorized scan for a specific case, say so.

**Mandatory callout triggers — add one whenever ANY of these are true:**

1. **Outperforms a known system** — NexusDB is faster, uses less memory, or requires
   fewer I/O operations than any named database or library in the reference table.
   → `callout-advantage`

2. **Avoids a known cost** — We eliminate something another system pays: double-write
   buffer (InnoDB), UNDO pass (PostgreSQL), compaction write amplification (RocksDB),
   double-buffering in RAM (InnoDB), global read locks (SQLite WAL mode), etc.
   → `callout-advantage`

3. **Borrowed a technique** — We adapted an approach from another system: PostgreSQL's
   physical WAL, RocksDB's LSM compaction ideas, CockroachDB's epoch-based reclamation,
   DuckDB's vectorized morsel execution, logos DFA lexer, etc. Explain where it came
   from and why we chose it. → `callout-design`

4. **A constant or limit was derived** — ORDER_INTERNAL = 223, PAGE_SIZE = 16 KB,
   u24 instead of u32, null bitmap instead of Option<T>. Show the derivation and the
   trade-off vs. the alternative. → `callout-design`

5. **A simpler design was explicitly rejected** — next_leaf not used, no UNDO pass,
   no nom combinators, no buffer pool. Explain what was rejected and why.
   → `callout-design`

6. **A benchmark result is ≥ 2× better** than a named competitor. → `callout-advantage`

**Where to place callouts:**

- In **technical docs** (`internals/`): immediately after the paragraph or code block
  that describes the notable thing. Not at the top of the page — at the exact point
  where the reader needs to understand why.
- In **user docs** (`user-guide/`): at the point of first contact with the feature,
  to help the reader understand what they get that they wouldn't get elsewhere.
- **Not inside code blocks.** Place them before or after, never inside a fenced block.
- **Maximum 2 callouts per section** (H2 or H3). If you have more, consolidate or
  pick the most impactful.

**Quality bar for callout content:**

- Name the competitor or alternative explicitly: "MySQL InnoDB", "PostgreSQL pg_wal",
  "RocksDB compaction", "SQLite 4 KB pages", "sqlparser-rs" — not vague comparisons.
- Quantify when possible: "2× disk writes", "9–17× faster", "1 GB disk savings at 100M rows".
- State the reason: "because X" or "eliminating Y overhead".
- One sentence is enough. Three is the max. Not a paragraph.

---

#### Proactive update rule

**If you modify, add, or delete any functionality that is already documented in `docs-site/`, you MUST update the relevant pages in the same commit.** No exceptions.

This applies to:
- Any change to a public struct, enum, trait, or function signature
- Any change to an on-disk format (page layout, WAL entry format, codec format)
- Any change to an algorithm (rebalance logic, recovery state machine, visibility function)
- Any change to SQL syntax accepted or rejected by the parser/analyzer
- Any new error type or changed error message
- Any benchmark result that changes by more than 5%

The review checklist (Step 1 of /review-task) includes checking that docs-site pages
were updated. A subphase cannot be closed if the docs are stale.

#### Which docs-site pages to update per component

| Component changed | User doc to update | Technical doc to update |
|---|---|---|
| SQL syntax (parser) | `sql-reference/ddl.md` or `dml.md` | `internals/sql-parser.md` |
| Expression evaluator | `sql-reference/expressions.md` | `internals/sql-parser.md` |
| Semantic analyzer | `sql-reference/` (affected statements) | `internals/semantic-analyzer.md` |
| Executor (Phase 5+) | `user-guide/getting-started.md`, `sql-reference/dml.md` | `internals/architecture.md` |
| Storage / page format | — | `internals/storage.md` |
| B+ Tree | `user-guide/features/indexes.md` | `internals/btree.md` |
| WAL / crash recovery | `user-guide/features/transactions.md` | `internals/wal.md` |
| MVCC / TxnManager | `user-guide/features/transactions.md` | `internals/mvcc.md` |
| Catalog | `user-guide/features/catalog.md` | `internals/catalog.md` |
| Row codec | — | `internals/row-codec.md` |
| New error type | `user-guide/errors.md` | — |
| Benchmark result | `user-guide/performance.md` | `development/benchmarks.md` |
| New phase completed | `development/roadmap.md` | All internals pages for that phase |

---

### Unimplemented items — anti-gap rule

If something cannot be fully implemented (scope, technical limitation, future dependency):

**DO NOT** leave it unmarked. Instead:

1. In the spec, add a section:
   ```
   ## ⚠️ DEFERRED
   - [description of the gap] → pending in subphase X.Y
   ```

2. In `docs/progreso.md`, add a line under the subphase:
   ```
   - [ ] ⚠️ [short description] — gap identified, revisit in [subphase]
   ```

**Why:** Silent gaps turn into bugs or duplicated work in future phases.
The goal is to have full visibility of the real state of the project at all times.

### Maximum implementation principle

Always implement the most complete and correct version possible, regardless of complexity.
Do not simplify for convenience. If something is difficult, find the correct way to do it.
Only mark as DEFERRED when there is a **real dependency** on another phase or a
documented external limitation — never due to complexity or convenience.

**Context gets compacted.** Write as if the reader was not present in this session.

### Commit format

```
feat(fase-N): concise description

- detail 1
- detail 2

Phase N/34 completed. See docs/fase-N.md
Spec: specs/fase-N/ | Tests: crates/dbyo-X/tests/
```

### GitHub account

Always use the **lordmacu** account (personal, NOT the work account).
The repo is configured with:
- `user.name = lordmacu`
- `user.email = lordmacu@users.noreply.github.com`
- GitHub CLI authenticated as lordmacu

Do not include Co-Authored-By from Claude in any commit of this project.

### Git branches

```
main              → stable code
fase-N-nombre     → phase development
hotfix/nombre     → urgent fixes on top of main
```

Never push directly to main. Merge only after /review-task is complete.

---

## /review-task — Audit before closing

**MANDATORY before closing any phase.** A closing commit cannot be made without passing this full review.

### Step 1 — Review by Explore subagent

Launch an Explore agent with instructions to review:

1. **Each acceptance criterion** from all specs of the phase — mark ✅/❌
2. **`unwrap()` in production code** (`src/`, excluding `#[cfg(test)]`) → blocker if found
3. **`unsafe` without `SAFETY:` comment** → blocker if found
4. **Integration tests** in `tests/` — do they exist? → blocker if not
5. **Benchmarks** in `benches/` — do they compile? → blocker if not
6. **Test logic** — are the assertions correct? Are there tests that always pass without verifying anything real?
7. **Unidentified gaps** — is there functionality promised in the spec that is not implemented?
8. **Documentation staleness** — for every component touched in this phase:
   - Is the corresponding `docs-site/src/internals/` page updated and accurate?
   - Is the corresponding `docs-site/src/user-guide/` page updated and accurate?
   - Are new error types added to `user-guide/errors.md`?
   - Are benchmark results updated in `user-guide/performance.md` and `development/benchmarks.md`?
   - Is `development/roadmap.md` updated with the new phase status?
   → Stale documentation is a blocker.

9. **Callout coverage** — for every doc page touched in this phase:
   - Does each mandatory callout trigger (outperforms a known system, avoids a known
     cost, borrowed a technique, derived a constant, rejected a simpler design,
     benchmark ≥ 2× better) have a corresponding callout in the doc?
   - Is the comparison against the **most relevant** system for the context — not
     always MySQL/PostgreSQL? (e.g., LSM comparisons → RocksDB; embedded → SQLite/DuckDB;
     Rust parsers → sqlparser-rs; distributed → CockroachDB/TiDB)
   - Are callouts placed at the right location (after the paragraph/block, not at
     the top of the page)?
   - Do callouts name the competitor explicitly and quantify the benefit?
   → Missing callouts for mandatory triggers are a blocker.

The subagent must return a report with:
- List of fulfilled/unfulfilled criteria per subphase
- List of blockers found (with file:line)
- List of gaps or deferred items

### Step 2 — Fix blockers

Fix **all** blockers before continuing. No exceptions.

### Step 3 — Mandatory benchmarks

**MANDATORY before closing.** Run the benchmarks for the phase and report results to the user.

```bash
cargo bench --bench [nombre_bench] 2>&1 | tee /tmp/bench-fase-N.txt
```

**Always report a comparison table with 6 columns:**

| Benchmark | NexusDB | MySQL (aprox) | PostgreSQL (aprox) | Target | Máx aceptable | Veredicto |
|---|---|---|---|---|---|---|
| point_lookup/1M | X ns / Y Mops | ~1.2 µs / ~830K ops | ~0.9 µs / ~1.1M ops | 800K ops/s | 600K ops/s | ✅/⚠️/❌ |
| range_scan/10K | X µs | ~8ms | ~5ms | 45ms | 60ms | ✅/⚠️/❌ |
| parser/simple_select | X ns | ~500ns | ~450ns | — | — | ✅/⚠️/❌ |
| insert/sequential | X ops/s | ~150K ops/s | ~120K ops/s | 180K ops/s | 150K ops/s | ✅/⚠️/❌ |

**Valores de referencia aproximados para MySQL y PostgreSQL** (actualizar si cambian):

| Operación | MySQL 8 (aprox) | PostgreSQL 15 (aprox) |
|---|---|---|
| Parser simple SELECT | ~450–600 ns | ~400–550 ns |
| Parser complex SELECT | ~3–6 µs | ~3–5 µs |
| Point lookup PK (1M rows, localhost) | ~100–300 µs (con red) / ~1–2 µs (en proc) | ~150–400 µs (con red) / ~0.8–1.2 µs (en proc) |
| Range scan 10K rows | ~5–15 ms | ~3–10 ms |
| INSERT con WAL | ~130–200K ops/s | ~100–160K ops/s |
| Seq scan 1M rows | ~0.5–1.5s | ~0.4–1.2s |
| Row codec encode | N/A (no expuesto) | N/A |
| Expr eval (scan 1K rows) | ~8–15M rows/s | ~5–12M rows/s |

**Nota sobre comparaciones:** MySQL/PostgreSQL se miden en proceso completo con WAL y red.
Nuestros benchmarks de parser y codec son puros (sin red, sin WAL). Para operaciones
que incluyen WAL (INSERT, UPDATE), la comparación es directa. Para parser/codec,
comparar con sqlparser-rs es más honesto que con MySQL/PG (que incluyen más overhead).

**Thresholds:**
- ✅ Supera MySQL o PostgreSQL, o cumple el target
- ⚠️ Dentro del máximo aceptable pero no alcanza el target — documentar en `docs/fase-N.md`
- ❌ Por debajo del máximo aceptable — **blocker**, investigar antes de continuar

Si hay un ❌, abrir `/debug` para identificar el bottleneck antes de continuar.

### Step 4 — Closing checklist
   ```
   [ ] All acceptance criteria from all specs ✅
   [ ] cargo test --workspace ✅
   [ ] cargo clippy -- -D warnings ✅
   [ ] cargo fmt --check ✅
   [ ] No unwrap() in src/ (only in tests and benches) ✅
   [ ] All unsafe has SAFETY: comment ✅
   [ ] Integration tests in tests/ ✅
   [ ] Benchmarks run and results reported to the user ✅
   [ ] No benchmark ❌ (blocker) ✅
   [ ] Test logic reviewed (no empty assertions) ✅
   [ ] docs/progreso.md updated ✅
   [ ] docs-site/src/internals/ updated for all components touched ✅
   [ ] docs-site/src/user-guide/ updated for all user-visible changes ✅
   [ ] development/roadmap.md phase status updated ✅
   [ ] Stale documentation checked (blocker if found) ✅
   [ ] Callouts added for every mandatory trigger in this subphase ✅
   [ ] Commit done ✅
   ```

---

## /debug — Systematic debugging

When something does not work:

1. **Reproduce** with the minimal test that demonstrates the bug
2. **Formulate hypotheses** — at least 2, do not assume the first
3. **Design an experiment** to validate/discard each hypothesis
4. **Fix in the right place** — do not patch symptoms
5. **Regression test** — make sure it never comes back

Never make random changes hoping it works.

---

## /bench — Compare performance

Before any optimization:

```bash
# 1. Baseline BEFORE the change
cargo bench --bench [nombre] > /tmp/before.txt

# 2. Make the change

# 3. Measure AFTER
cargo bench --bench [nombre] > /tmp/after.txt

# 4. Compare
cargo install critcmp
critcmp /tmp/before.txt /tmp/after.txt
```

If there is a regression > 5% on a critical operation: blocker.

### Performance budget (do not regress)

| Operation             | NexusDB actual | MySQL (aprox) | PostgreSQL (aprox) | Target       | Máx aceptable   |
|-----------------------|----------------|---------------|--------------------|--------------|-----------------|
| Point lookup PK       | TBD (Phase 4.5)| ~830K ops/s   | ~1.1M ops/s        | 800K ops/s   | 600K ops/s      |
| Range scan 10K rows   | 0.61ms ✅      | ~8ms          | ~5ms               | 45ms         | 60ms            |
| INSERT with WAL       | TBD (Phase 4.5)| ~150K ops/s   | ~120K ops/s        | 180K ops/s   | 150K ops/s      |
| Seq scan 1M rows      | TBD (Phase 4.5)| ~0.8s         | ~0.5s              | 0.8s         | 1.2s            |
| Concurrent reads x16  | TBD (Phase 7)  | ~2x degradation| ~1.5x degradation  | linear       | <2x degradation |
| Parser simple SELECT  | **500ns** ✅   | ~500ns        | ~450ns             | —            | —               |
| Parser complex SELECT | **2.7µs** ✅   | ~4µs          | ~3.5µs             | —            | —               |
| Row codec encode      | **30ns** ✅    | N/A           | N/A                | —            | —               |
| Expr eval scan/1K     | **14.8M/s** ✅ | ~8M rows/s    | ~6M rows/s         | —            | —               |

Actualizar columna "NexusDB actual" con cada benchmark completado.
TBD = requiere executor (Phase 4.5) para medirse con I/O real.

---

## /unsafe-review — Audit unsafe blocks

For each `unsafe` block in the code:

```
1. Why is it necessary? Does a safe alternative exist?
   → Try bytemuck, rkyv, or restructure first

2. What invariant guarantees it is safe?
   → Document with a SAFETY: comment

3. Is there a test that verifies the contract?
   → If not: write it before merging

4. Is it encapsulated in a public safe function?
   → The caller should not see unsafe
```

```rust
// Mandatory format:
// SAFETY: [invariant that guarantees this code is safe]
// Specifically: [what condition must hold]
let page = unsafe { &*(ptr as *const Page) };
```

---

## /new-crate — Add a crate to the workspace

```bash
# 1. Create structure
cargo new --lib crates/dbyo-X

# 2. Add to the workspace in the root Cargo.toml
# members = [..., "crates/dbyo-X"]

# 3. Define ONLY public types and traits in src/lib.rs

# 4. Write initial test (empty but compiling)

# 5. Update memory/architecture.md
```

Verify there are no circular dependencies:
```bash
cargo tree --workspace | grep "dbyo-X"
```

---

## /profile — Find real bottlenecks

```bash
# Install tools once
cargo install flamegraph
cargo install cargo-samply

# Profile with flamegraph
cargo flamegraph --bench [nombre_bench]
open flamegraph.svg

# Or with samply (better on macOS)
cargo samply record cargo bench --bench [nombre]
```

Process:
1. Benchmark that reproduces the slow case
2. Flamegraph → most costly function
3. Optimize ONLY that function
4. Verify the benchmark improves
5. Verify nothing else regressed

---

## /fuzz — Testing with random inputs

Critical for the SQL parser and the storage engine:

```bash
# Install cargo-fuzz
cargo install cargo-fuzz

# Create fuzz target
cargo fuzz add fuzz_sql_parser
cargo fuzz add fuzz_storage_pages
cargo fuzz add fuzz_wal_recovery

# Run (minimum 60 seconds in CI, more locally)
cargo fuzz run fuzz_sql_parser -- -max_total_time=300

# For each crash found:
# 1. Add as a regression test in tests/
# 2. Fix the bug
# 3. Verify the test passes
```

---

## /checkpoint — Save context when pausing

When a task must be paused:

```bash
# Create checkpoint
cat > docs/checkpoint-$(date +%Y%m%d).md << 'EOF'
# Checkpoint [date]

## What was being done
[exact description]

## Pending decision
[what needed to be decided]

## Exact next step
[precise instruction to continue]

## Files modified so far
[list]

## Tests failing / passing
[current state]
EOF

git add -A && git commit -m "checkpoint: pausar en [descripción]"
```

Next session: read the checkpoint before anything else.

---

## /invariant — Verify database invariants

After complex operations, verify the engine is consistent:

```rust
// Invariants that must always hold:

// 1. B+ Tree balanced
assert!(btree.all_leaves_same_depth());

// 2. Free list with no duplicates
assert!(free_list.has_no_duplicates());

// 3. No page referenced twice
assert!(page_refs.no_double_references());

// 4. WAL LSN always increasing
assert!(wal.is_monotonic());

// 5. All FKs point to existing rows
assert!(fk_checker.all_valid());

// 6. Checksum of every page is correct
assert!(storage.all_checksums_valid());
```

```bash
# SQL command to verify
SELECT * FROM db_integrity_check();
-- Returns: OK or list of violations
```

---

## Rust code conventions

### Errors — always thiserror

```rust
// CORRECT
#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("page {page_id} not found")]
    PageNotFound { page_id: u64 },

    #[error("invalid checksum on page {page_id}: expected {expected}, got {got}")]
    ChecksumMismatch { page_id: u64, expected: u32, got: u32 },
}

// FORBIDDEN in src/ — only in tests and benches
let page = storage.read_page(42).unwrap();

// CORRECT
let page = storage.read_page(42)?;
let page = storage.read_page(42).map_err(|e| DbError::StorageError(e))?;
```

### Async vs sync

```rust
// I/O and network connections → tokio async
async fn handle_connection(stream: TcpStream) -> Result<()> { ... }

// CPU intensive → rayon (DO NOT block the tokio runtime)
fn parallel_scan(table: &Table) -> Vec<Row> {
    table.morsels(100_000).par_iter().flat_map(|m| scan(m)).collect()
}

// Call rayon from tokio:
let result = tokio::task::spawn_blocking(|| parallel_scan(table)).await?;
```

### Unsafe — always with SAFETY

```rust
// CORRECT
// SAFETY: `ptr` points to a valid page within the mmap.
// The mmap lives as long as `StorageEngine` exists (guaranteed by Arc).
// page_id < total_pages verified before this call.
let page = unsafe { &*(ptr as *const Page) };

// FORBIDDEN — unsafe without justification
let page = unsafe { &*(ptr as *const Page) };
```

### Testing pyramid

```rust
// Unit: fast, no I/O, use MemoryStorage
#[test]
fn test_btree_insert() {
    let storage = MemoryStorage::new();
    let tree = BTree::new(storage);
    tree.insert(b"key1", RecordId(1)).unwrap();
    assert_eq!(tree.lookup(b"key1").unwrap(), Some(RecordId(1)));
}

// Integration: real I/O, crash recovery
#[test]
fn test_crash_recovery() {
    let dir = tempfile::tempdir().unwrap();
    // Write data
    let engine = Engine::open(dir.path()).unwrap();
    engine.execute("INSERT INTO t VALUES (1)").unwrap();
    drop(engine); // simulate crash

    // Re-read — must recover
    let engine = Engine::open(dir.path()).unwrap();
    let rows = engine.execute("SELECT * FROM t").unwrap();
    assert_eq!(rows.len(), 1);
}

// Bench: measure, do not verify
#[bench]
fn bench_point_lookup(b: &mut Bencher) {
    let engine = setup_bench_engine();
    b.iter(|| engine.execute("SELECT * FROM users WHERE id = 42"))
}
```

---

## Project structure

```
dbyo/
├── CLAUDE.md              ← this file (workflow)
├── db.md                  ← complete design (source of truth)
├── Cargo.toml             ← workspace root
├── .claude/
│   └── settings.json      ← project hooks (fmt + clippy)
├── specs/                 ← specs and plans per phase
│   └── fase-01/
│       ├── spec-storage.md
│       └── plan-storage.md
├── docs/                  ← documentation of what is implemented
│   ├── README.md
│   └── fase-01.md         ← created when the phase is completed
├── crates/                ← code
│   ├── dbyo-core/
│   ├── dbyo-storage/
│   └── ...
├── tests/                 ← integration tests
├── benches/               ← benchmarks with criterion
└── fuzz/                  ← cargo-fuzz targets
```

---

## Memory protocol — update when completing each phase

Files in `.claude/projects/-Users-cristian-dbyo/memory/`:

| File | When to update |
|---|---|
| `project_state.md` | Always when closing a phase |
| `architecture.md` | When a crate is created or modified |
| `decisions.md` | When an important technical decision is made |
| `lessons.md` | When something surprising happens (good or bad) |

---

## Before each session

```
1. read docs/checkpoint-*.md if it exists (continue from there)
2. read docs/fase-N.md from the last completed phase
3. read specs/fase-actual/ to recall the spec
4. run: cargo test --workspace
5. continue from where it was left off
```
