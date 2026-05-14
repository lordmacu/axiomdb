---
name: scan-gaps
description: Discover undocumented gaps by comparing AxiomDB against MySQL/PostgreSQL — build inventory, run tests, classify, hand off to hunt-gap
---

# /scan-gaps — Discover undocumented gaps by comparing with real databases

Proactively find gaps, bugs, and missing features that are NOT yet documented
in `docs/progreso.md`. Compares AxiomDB against MySQL, PostgreSQL, MariaDB,
and SQL standards to find what's missing or broken.

## Invocation modes

```
/scan-gaps fase 3              — scan phase 3 features against reference DBs
/scan-gaps fase 1-11           — scan phases 1 through 11
/scan-gaps area parser         — scan parser for SQL syntax gaps
/scan-gaps area wire           — scan MySQL wire protocol for compat gaps
/scan-gaps area executor       — scan executor for correctness gaps
/scan-gaps area storage        — scan storage/WAL for robustness gaps
/scan-gaps area types          — scan type system for missing types/casts
/scan-gaps full                — comprehensive scan of everything
```

---

## Phase 1 — Build the feature inventory

### Step 1: Read what was implemented

For the target phase(s) or area:

1. Read `docs/progreso.md` — list all `[x] ✅` items (completed features)
2. Read `docs/fase-N.md` — understand what was built
3. Read `specs/fase-N/` — understand what was specified
4. Grep the codebase for the feature's implementation

### Step 2: Build a feature checklist

For each completed feature, create a checklist of what MySQL/PostgreSQL
expects for that feature. Sources:

- `research/postgresql/` — PostgreSQL source code
- `research/mariadb/` — MariaDB source code  
- `research/sqlite/` — SQLite source code
- `research/duckdb/` — DuckDB source code
- SQL standard knowledge (SQL:2016, SQL:2023)
- MySQL 8.0 reference manual behavior

Example for "Phase 4 - SQL Parser":
```
Feature: SELECT statement
  MySQL expects:
    [?] SELECT ... UNION [ALL] SELECT ...
    [?] SELECT ... INTERSECT SELECT ...
    [?] SELECT ... EXCEPT SELECT ...
    [?] SELECT INTO @var
    [?] SELECT ... FOR UPDATE SKIP LOCKED
    [?] SELECT ... WINDOW w AS (...)
    [?] Parenthesized SELECT: (SELECT 1) UNION (SELECT 2)
```

### Step 3: Identify what to test

Mark each checklist item as:
- **[skip]** — explicitly deferred to a later phase (check progreso.md)
- **[test]** — should work based on the completed features, needs verification
- **[missing]** — clearly not implemented, would be a new gap

Focus on `[test]` items — these are the ones that MIGHT be broken.

---

## Phase 2 — Create verification scripts

For each `[test]` item, create `tools/tmp_gap_test.py` using the same
template as `/hunt-gap` Phase 2B.

### Test design strategy by area

**Parser gaps** — try SQL syntax that MySQL accepts:
```python
# Does AxiomDB parse this without error?
try:
    cur.execute("THE SQL SYNTAX TO TEST")
    ok("syntax accepted", True)
except Exception as e:
    if "syntax" in str(e).lower() or "1064" in str(e):
        ok("syntax accepted", False, got=str(e))  # parser gap
    else:
        ok("syntax accepted but execution failed", False, got=str(e))  # executor gap
```

**Wire protocol gaps** — test MySQL client expectations:
```python
# Does the wire protocol behave like MySQL?
import struct
# Check column types, character sets, status flags, etc.
```

**Type system gaps** — test implicit casts and coercions:
```python
# Does AxiomDB handle type X correctly?
cur.execute("CREATE TABLE t (col SOMETYPE)")
cur.execute("INSERT INTO t VALUES (literal)")
cur.execute("SELECT col FROM t WHERE col = literal")
```

**Executor correctness gaps** — test edge cases:
```python
# Does this query return the correct result?
cur.execute("COMPLEX QUERY WITH EDGE CASE")
rows = cur.fetchall()
ok("correct result", rows == expected, got=rows)
```

**Storage robustness gaps** — test crash recovery, corruption handling:
```python
# More complex — may need to kill/restart the server mid-operation
```

### Run the tests

```bash
export PATH="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH"
cargo build -p axiomdb-server
python3 tools/tmp_gap_test.py
```

---

## Phase 3 — Classify discoveries

After running the tests, classify each finding:

| Category | Meaning | Action |
|----------|---------|--------|
| **NEW GAP** | Feature should work but doesn't | Document + fix via `/hunt-gap` |
| **KNOWN GAP** | Already in progreso.md as `⏳` | Skip — already tracked |
| **DEFERRED** | Explicitly planned for later phase | Skip — note phase |
| **WORKS** | Feature works correctly | Remove from checklist |
| **EDGE CASE** | Works for basic case, fails on edge | Document as new gap |

### Report format

```
SCAN RESULTS: [area/phase]
Date: YYYY-MM-DD

Features tested: X
  Working correctly: Y
  Known gaps (already tracked): Z
  NEW gaps discovered: W

NEW GAPS:
  1. [severity] [area] — [description]
     Test: [SQL that fails]
     Expected: [what MySQL/PG does]
     Got: [what AxiomDB does]

  2. [severity] [area] — [description]
     ...

EDGE CASES:
  1. [description] — [basic case works, edge case fails]
     ...
```

---

## Phase 4 — Document and fix

### Step 1: Add new gaps to progreso.md

For each NEW GAP, add an entry under the appropriate section:

```markdown
- [ ] GAP-X.N ⏳ [description] — discovered by /scan-gaps [date]
```

### Step 2: Prioritize

Sort new gaps by:
1. **Critical** — causes crash/panic/data loss
2. **High** — blocks ORMs/clients, wrong results
3. **Medium** — missing SQL feature, compatibility issue  
4. **Low** — cosmetic, edge case, rare scenario

### Step 3: Fix using /hunt-gap

For each new gap, in priority order:
1. Delete the scan test file
2. Run `/hunt-gap [gap-id]` to verify and fix it
3. Continue to the next gap

### Step 4: Clean up

```bash
rm tools/tmp_gap_test.py
```

---

## Phase 5 — Cross-reference with research

For SQL semantics questions, compare behavior directly:

### PostgreSQL reference patterns

```bash
# Find how PostgreSQL implements feature X
grep -r "keyword" research/postgresql/src/backend/parser/
grep -r "keyword" research/postgresql/src/backend/executor/
```

### MariaDB reference patterns

```bash
grep -r "keyword" research/mariadb/sql/
```

### SQLite reference patterns

```bash
grep -r "keyword" research/sqlite/src/
```

### DuckDB reference patterns

```bash
grep -r "keyword" research/duckdb/src/
```

### What to look for in reference code

| Question | Where to look |
|----------|---------------|
| Does MySQL support this syntax? | MariaDB `sql/sql_yacc.yy` |
| How does PG handle this edge case? | PostgreSQL `src/backend/executor/` |
| What's the correct NULL behavior? | SQL standard + PG `src/backend/utils/adt/` |
| How does wire protocol encode this? | MariaDB `sql/net_serv.cc`, `sql/protocol.cc` |
| What type coercions are expected? | PG `src/backend/parser/parse_coerce.c` |

---

## Rules

1. **Verify before documenting.** Don't assume something is broken — test it.
2. **Compare with MySQL first.** AxiomDB targets MySQL compatibility primarily.
3. **Use research/ for authoritative answers.** Don't guess SQL semantics.
4. **One test script per scan.** Don't create multiple files — use one
   `tools/tmp_gap_test.py` with all the tests for the current scan.
5. **Clean up always.** Delete test files after the scan.
6. **Hand off to /hunt-gap.** This skill discovers; `/hunt-gap` fixes.
7. **Don't fix during scan.** Complete the full scan first, document all
   findings, THEN fix in priority order.
8. **Track what you tested.** The scan report should list everything checked,
   not just the failures — so future scans don't repeat work.
