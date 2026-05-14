# AxiomDB — Database Engine in Rust

## Source of truth
`db-summary.md` — compact orientation (read this first, ~100 lines).
`db.md` — full design reference; grep specific sections as needed, never read whole file.

---

## Decision-making style

Make decisions autonomously. Only ask when there is a genuine blocker that cannot be resolved
without user input. One question max per session, and only if truly necessary.

---

## Mandatory engineering workflow

```
/brainstorm → /spec-task → /plan-task → /implement-task → /review-task
```

Never skip steps. Applies **per subphase** (3.1, 3.2…). Each subphase has its own spec, plan,
implementation, and review before moving to the next.

---

## Effort levels

| Level | When to use |
|---|---|
| `max` | Novel algorithm, safety-critical (crash recovery, MVCC, concurrency), unsafe code, major downstream impact |
| `high` | Complex feature with non-obvious edge cases, significant data structure design, multi-component integration |
| `medium` | Standard implementation of a fully-defined spec, incremental features |
| `low` | Mechanical work: tests, docs, minor additions, format changes |

At the **start of /brainstorm**: recommend effort for each step (Brainstorm+Spec / Plan / Impl / Review).
At the **end of /spec-task**: tell the user which effort to use for Plan.

---

## /implement-task

### ⚡ MANDATORY: Declare effort BEFORE writing any code

```
⚡ Effort required: [medium | high | max]
Reason: [one line]
```

Then STOP and wait for user confirmation before writing a single line of code.

### Validation during implementation

- Run tests for the crate you touched: `cargo nextest run -p axiomdb-...` (on Lima VM)
- Run `tools/wire-test.py` only if the change is observable through the MySQL wire protocol
- Run `cargo nextest run --workspace` only when **closing** the subphase/phase
- Expand beyond the touched crate when: public API/trait changed, on-disk format changed,
  SQL semantics changed, or shared crates (`axiomdb-core`, `axiomdb-types`, `axiomdb-storage`) changed

### Closing protocol — mandatory on every subphase

```
1.  cargo nextest run --workspace (Lima VM) — clean
2.  cargo clippy --workspace -- -D warnings (Lima VM) — clean
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

Update `docs-site/` on every subphase close:

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
outperforming a named system, avoiding a known cost, borrowing a technique, or benchmark ≥2×.
Name the competitor explicitly + quantify.

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
```

Do not include Co-Authored-By from Claude.

### GitHub account

Use the **lordmacu** account: `user.name = lordmacu`, `user.email = lordmacu@users.noreply.github.com`.

### Git branches

```
main          → stable code
fase-N-nombre → phase development
hotfix/nombre → urgent fixes
```

---

## /scan-gaps and /hunt-gap — proactive triggers

Activate `/scan-gaps` automatically when:
- User says "busca gaps", "find bugs", "scan for issues", "compare with MySQL"
- After closing a major phase

Activate `/hunt-gap` automatically when:
- User mentions **gaps**, **bugs**, **unwrap**, **panics**, or **missing features**
- User says "fix", "solve", "hunt", "find bugs", "verify gaps"
- During `/review-task` if acceptance criteria reveal a gap not in progreso.md
- During `/implement-task` if a test failure reveals an undocumented bug
- When reading `docs/progreso.md` and seeing `⏳` items in phases already closed

When triggered proactively, announce it: `Detected [trigger]. Activating /hunt-gap for [gap-id].`

---

## Rust code conventions

- **Errors**: use `thiserror`. No `unwrap()` in `src/` — use `?` or `map_err(...)`.
- **Async**: I/O → tokio async. CPU-intensive → rayon + `spawn_blocking`. Never block tokio runtime.
- **Unsafe**: always requires `// SAFETY: [invariant]` comment. No exceptions.
- **Testing**: unit tests use `MemoryStorage` (no I/O). Integration tests use real I/O + `tempfile`.
  Benchmarks measure only — no assertions.
- **Lima VM**: all `cargo build/test/clippy` runs on Lima VM "axiomdb". Never on macOS local.
  Exception: `cargo build -p axiomdb-server` for macOS wire test binary.

---

## Project structure

```
axiomdb/
├── CLAUDE.md, db.md, db-summary.md   ← orientation
├── Cargo.toml                         ← workspace root
├── specs/fase-N/                      ← specs and plans
├── docs/                              ← phase docs + progreso.md
├── crates/                            ← source code
├── tools/                             ← wire-test.py, scripts
├── tests/                             ← integration tests
├── benches/                           ← criterion benchmarks
└── fuzz/                              ← cargo-fuzz targets
```

---

## Memory protocol

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
