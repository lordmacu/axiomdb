# AxiomDB — Agent Instructions

AxiomDB is a SQL database engine written in Rust (MySQL-wire-protocol
compatible). This file tells any AI agent working on the project what to
read first, which skills to load, and how to behave.

The detailed workflow, effort levels, performance budget and conventions
live in `CLAUDE.md` — read it when you need full context. This file is
the short, operational entry point.

---

## Read these first (every session)

In this exact order, before doing anything else:

1. `db-summary.md` — 100-line orientation of the whole design
2. `memory/project_state.md` — current active phase, last closed subphase, open gaps
3. `docs/progreso.md` — subphase-by-subphase progress (status markers ⏳ 🔄 ✅ ⏸)
4. `docs/checkpoint-*.md` if present — the previous session paused here
5. Only if needed for the current task: `db.md` (10K lines — grep, never read whole),
   `docs/fase-N.md` for a specific phase, or `specs/fase-N/` for a specific task

Never ask the user what phase we are in — read `memory/project_state.md`.

---

## Skills (load when their trigger matches)

Skills live in two mirrored locations:

- `.claude/skills/<name>/SKILL.md` — project-local (source of truth, version-controlled)
- `~/.hermes/skills/axiomdb/<name>/SKILL.md` — Hermes global (mirror, for `/skill` command)

Load a skill whenever its trigger matches, using the tool available to you
(`skill_view` in Hermes, automatic in Claude Code). Do not improvise a
workflow if a skill exists for it.

| Skill | Load when the user says / the task is about |
|---|---|
| `brainstorm` | "how should we...", "let's explore", starting any non-trivial task or new subphase |
| `spec-task` | writing a spec for a task, after brainstorm is agreed |
| `plan-task` | breaking an approved spec into TDD steps |
| `debug` | a bug, panic, test failure, "doesn't work", "investigate" |
| `bench` | performance, "measure", "slow", "fast", "compare with MySQL/PostgreSQL" |
| `fuzz` | parser hardening, "fuzz", "malformed input", cargo-fuzz |
| `unsafe-review` | unsafe blocks, "audit", reviewing any PR that touches unsafe |
| `new-crate` | creating a new `axiomdb-*` crate in the workspace |
| `checkpoint` | pausing a task mid-way, context nearly full, long break |
| `subfase-completa` | closing a subphase N.M (tests, docs, memory, commit) |
| `fase-completa` | closing a full phase (the final gate) |
| `scan-gaps` | "find gaps", "compare with MySQL", proactive compat audit |
| `hunt-gap` | "fix GAP-X.Y", "hunt bugs", processing items from progreso.md `⏳` |

Proactive triggers: If the user mentions **gaps**, **bugs**, **unwrap**, **panics**,
**missing features**, **compat**, load `scan-gaps` / `hunt-gap` automatically and
announce it before acting.

---

## Mandatory engineering workflow

```
brainstorm → spec-task → plan-task → implement → review → subfase-completa
```

Applied **per subphase** (e.g. 11.25a, not per entire phase 11). Never skip.

Before writing code in `implement`, **declare effort** and WAIT for user
confirmation:

```
⚡ Effort required: [medium | high | max]
Reason: [one line]
```

Effort scale: `max` (novel / safety-critical / unsafe) → `high` (complex,
edge cases) → `medium` (standard spec impl) → `low` (tests, docs, mechanical).

---

## Core conventions (full detail in CLAUDE.md)

- **Rust errors**: `thiserror`, `Result`, `?`. **No `unwrap()` / `expect()` in `src/`**.
- **Unsafe**: every block needs `// SAFETY: [specific invariant]` before it. No exceptions.
- **Async**: I/O → tokio. CPU → rayon + `spawn_blocking`. Never block the tokio runtime.
- **Tests**: unit tests use `MemoryStorage` (no I/O). Integration tests in `tests/`
  use `tempfile`. Benchmarks have **no assertions**.
- **Targeted testing**: `cargo test -p axiomdb-CRATE` during implementation.
  `cargo test --workspace` **only** at subphase/phase close.
- **Wire test**: `python3 tools/wire-test.py` when the change is visible through
  the MySQL wire protocol.

## Commit format

```
feat(fase-N): concise description

- detail 1
- detail 2

Phase N/34. See docs/fase-N.md
Spec: specs/fase-N/ | Tests: crates/axiomdb-X/tests/
```

GitHub account: **lordmacu** (`lordmacu@users.noreply.github.com`).
Push target: `origin main`. Do not add Co-Authored-By from any assistant.

---

## Project structure

```
axiomdb/
├── AGENTS.md              ← this file (agent entry point)
├── CLAUDE.md              ← full workflow reference
├── db-summary.md          ← 100-line orientation
├── db.md                  ← full design (grep, never read whole)
├── Cargo.toml             ← workspace root
├── crates/axiomdb-*/      ← source code (one crate per component)
├── specs/fase-N/          ← spec-<name>.md + plan-<name>.md per task
├── docs/
│   ├── progreso.md        ← subphase progress tracker
│   ├── fase-N.md          ← per-phase closing document
│   └── checkpoint-*.md    ← session-pause snapshots
├── memory/
│   ├── project_state.md   ← current active phase + subphase
│   ├── architecture.md    ← crate map + invariants
│   └── lessons.md         ← retrospective learnings
├── benches/comparison/    ← 3-Docker MySQL/PG/AxiomDB harness
├── tools/
│   ├── wire-test.py       ← MySQL-wire regression suite
│   └── tmp_gap_test.py    ← temporary gap reproduction (delete after use)
├── fuzz/                  ← cargo-fuzz targets
└── research/              ← PG/MariaDB/SQLite/DuckDB source for reference
```

---

## Performance budget (never regress)

| Operation | Target | Max acceptable |
|---|---|---|
| Point lookup PK (Phase 5+) | 800K ops/s | 600K ops/s |
| Range scan 10K rows | 45ms | 60ms |
| INSERT with WAL | 180K ops/s | 150K ops/s |
| Seq scan 1M rows | 0.8s | 1.2s |

Full comparison with MySQL/PostgreSQL in the `bench` skill.
Regression > 5% on a critical op is a blocker.

---

## When you are unsure

- **Phase / state question** → `memory/project_state.md`
- **Subphase status** → `docs/progreso.md`
- **Workflow detail** → `CLAUDE.md`
- **SQL semantics** → `research/postgresql/`, `research/mariadb/`, `research/sqlite/`, `research/duckdb/`
- **Past phase closure** → `docs/fase-N.md`
- **Current task spec/plan** → `specs/fase-N/`
- **Don't know what to do next** → read the skills table above, pick the one whose
  trigger matches best, load it, follow it. The skill has the answer.

Decide autonomously when the information is available. Ask the user only
when there is a genuine blocker you cannot resolve by reading the sources above.
One question maximum per session.
