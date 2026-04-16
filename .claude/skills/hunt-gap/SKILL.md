---
name: hunt-gap
description: Verify and fix documented gaps one at a time — reproduce, root cause, minimal fix, wire-test regression, close
---

# /hunt-gap — Proactive gap/bug hunter

Systematically verify and fix documented gaps or bugs. Never assume a gap still
exists — always reproduce first with real code against the running server.

## Invocation modes

```
/hunt-gap GAP-A.1              — single gap by ID
/hunt-gap 11.2d                — single gap by subphase
/hunt-gap UNION support        — single gap by description
/hunt-gap fase 3               — scan ALL gaps in phase 3
/hunt-gap fase 1-11            — scan ALL gaps in phases 1 through 11
/hunt-gap fase 39              — scan ALL gaps in phase 39
/hunt-gap all                  — scan ALL gaps in docs/progreso.md
```

## Phase 0 — Scan mode (when a phase range is given)

When the argument is a phase number or range (not a specific gap ID):

1. **Read `docs/progreso.md`** and collect every `[ ] ⏳` item within the
   requested phase range. Also collect `⚠️ DEFERRED` items from
   `specs/fase-N/` and `docs/fase-N.md` files in scope.

2. **Build a gap inventory** — list all gaps found with:
   ```
   SCAN: Phase [N] to [M]
   Found [X] gaps:
     1. [id] — [one-line description] — [severity estimate]
     2. [id] — [one-line description] — [severity estimate]
     ...
   ```

3. **Suggest priority order** based on:
   - Severity (critical > high > medium > low)
   - Fix effort (quick wins first within same severity)
   - Dependencies (if gap B depends on gap A being fixed first)

4. **Process one at a time** — for each gap in order, run Phase 1 through
   Phase 6 below. After closing one gap, move to the next. Report progress:
   ```
   Gap hunt progress: [X/Y] gaps processed
     Closed: [list]
     Already fixed: [list]
     Skipped (needs other phase): [list]
   ```

5. **Ask before proceeding** to the next gap after each close (unless the
   user said to process all automatically).

---

## Phase 1 — Locate and understand the gap

1. **Find the gap record** — search in order:
   - `docs/progreso.md` — look for the gap ID or keywords
   - `specs/fase-N/` — detailed spec if it exists
   - `docs/fase-N.md` — phase docs for context
   - Source code — grep for TODO/FIXME/HACK related to the gap

2. **Document the gap** — before doing anything, state clearly:
   ```
   GAP: [identifier]
   SYMPTOM: [what the user would observe]
   EXPECTED: [what should happen instead]
   LOCATION: [file(s) and line(s) involved]
   SEVERITY: [critical / high / medium / low]
   TYPE: [code-level | wire-visible | parser | executor | storage]
   ```

3. **Check if already fixed** — grep the codebase and git log for evidence
   that this was already resolved in a later phase:
   ```bash
   git log --oneline --all --grep="GAP-X" | head -5
   ```
   If fixed, update `docs/progreso.md` and skip to Phase 6 (report only).

4. **Classify the gap type** — this determines Phase 2 strategy:
   - **Code-level** (unwrap/expect/unsafe in src/) → Phase 2A (grep verification)
   - **Wire-visible** (SQL features, protocol, types) → Phase 2B (Python test)
   - **Parser/AST** (syntax not recognized) → Phase 2B + check parser code
   - **Executor** (wrong results, missing logic) → Phase 2B + trace execution path
   - **Storage/format** (corruption, encoding) → Phase 2A + Phase 2B combined

---

## Phase 2A — Verify code-level gaps (unwrap/expect/unsafe)

For gaps that are purely about code patterns in `src/`:

1. **Grep for the exact pattern** described in the gap:
   ```bash
   # Count occurrences
   grep -rn 'unwrap()' crates/axiomdb-sql/src/target_file.rs
   grep -rn 'expect(' crates/axiomdb-sql/src/target_file.rs
   ```

2. **Read each occurrence** — determine if it's:
   - **Guarded** (preceded by a length/None check) → still a gap, lower priority
   - **Unguarded** (can actually panic) → critical, fix immediately
   - **In test code** (`#[cfg(test)]`) → not a gap, skip

3. **Report findings**:
   ```
   PATTERN: [what was found]
   COUNT: [number of occurrences]
   GUARDED: [yes/no — has preceding safety check]
   VERDICT: CONFIRMED / ALREADY-FIXED
   ```

4. If CONFIRMED → skip to Phase 5 (implementation is straightforward for
   these: replace `unwrap()` with `map_err(|_| error)?` or `?`).

---

## Phase 2B — Reproduce wire-visible gaps with Python test

### Step 1: Build the server

```bash
export PATH="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH"
cargo build -p axiomdb-server
```

### Step 2: Create the test script

Create `tools/tmp_gap_test.py` with this template:

```python
#!/usr/bin/env python3
"""Temporary gap verification — GAP-X.Y: [description]"""
import os, signal, subprocess, sys, tempfile, time, socket
import pymysql

PORT = 13307
_server_proc = None
_data_dir = None

def start_server():
    global _server_proc, _data_dir
    debug = "target/debug/axiomdb-server"
    release = "target/release/axiomdb-server"
    if os.path.isfile(debug) and os.path.isfile(release):
        binary = debug if os.path.getmtime(debug) > os.path.getmtime(release) else release
    elif os.path.isfile(release):
        binary = release
    elif os.path.isfile(debug):
        binary = debug
    else:
        print("Server binary not found — build first")
        sys.exit(1)
    _data_dir = tempfile.mkdtemp(prefix="axiomdb-gap-")
    env = os.environ.copy()
    env["AXIOMDB_DATA"] = _data_dir
    env["AXIOMDB_PORT"] = str(PORT)
    _server_proc = subprocess.Popen(
        [binary], env=env,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
    )
    for _ in range(50):
        try:
            with socket.create_connection(("127.0.0.1", PORT), timeout=0.1):
                return
        except OSError:
            time.sleep(0.1)
    stop_server()
    print(f"Server did not start on :{PORT} within 5s")
    sys.exit(1)

def stop_server():
    global _server_proc, _data_dir
    if _server_proc:
        _server_proc.terminate()
        try: _server_proc.wait(timeout=3)
        except subprocess.TimeoutExpired: _server_proc.kill()
        _server_proc = None
    if _data_dir and os.path.isdir(_data_dir):
        import shutil
        shutil.rmtree(_data_dir, ignore_errors=True)
        _data_dir = None

PASS = 0
FAIL = 0

def ok(label, cond, got=None):
    global PASS, FAIL
    if cond:
        print(f"  PASS {label}")
        PASS += 1
    else:
        detail = f" (got: {got!r})" if got is not None else ""
        print(f"  FAIL {label}{detail}")
        FAIL += 1

def connect():
    return pymysql.connect(
        host="127.0.0.1", port=PORT, user="root", password="",
        autocommit=False,
    )

def test_gap():
    """GAP-X.Y: [description]"""
    print("\n-- GAP-X.Y: [title] --")
    c = connect()
    cur = c.cursor()

    # Setup
    cur.execute("CREATE DATABASE IF NOT EXISTS gap_test")
    cur.execute("USE gap_test")

    # --- Control test (known-working feature to confirm infra works) ---
    # ok("control: SELECT works", ...)

    # --- Gap test (the feature/behavior that is expected to fail) ---
    try:
        pass
    except Exception as e:
        ok("gap description", False, got=str(e))

    # Cleanup
    for tbl in ["table1", "table2"]:
        try: cur.execute(f"DROP TABLE IF EXISTS {tbl}")
        except: pass
    c.commit()
    c.close()
    c2 = connect()
    try:
        c2.cursor().execute("DROP DATABASE gap_test")
        c2.commit()
    except: pass
    c2.close()

if __name__ == "__main__":
    start_server()
    try:
        test_gap()
    finally:
        stop_server()
    print(f"\n{'='*60}")
    print(f"  Result: {PASS} passed, {FAIL} failed")
    print(f"{'='*60}")
    sys.exit(1 if FAIL else 0)
```

### Step 3: Design the test cases

Every test must include these 3 categories:

| Category | Purpose | Example |
|----------|---------|---------|
| **Control** | Proves test infra works | `SELECT 1+1` → 2 |
| **Gap test** | Exercises the documented gap | `SELECT 1 UNION SELECT 2` → error? |
| **Edge case** | Boundary conditions | `UNION ALL` with NULLs, empty sets |

### Step 4: Run the test

```bash
python3 tools/tmp_gap_test.py
```

### Step 5: Analyze failures

For each failure, determine:
- **Expected failure** (gap is confirmed) → proceed to Phase 3
- **Unexpected failure** (found a different bug) → document as collateral
  finding, stay focused on original gap
- **Unexpected success** (gap was already fixed) → mark as ALREADY-FIXED

---

## Phase 3 — Classify the result

Report:

```
VERIFICATION RESULT for GAP-X.Y:
  Status: CONFIRMED / ALREADY-FIXED / PARTIALLY-FIXED
  Tests: X passed, Y failed
  Collateral findings: [unexpected bugs discovered, if any]
```

**If ALREADY-FIXED**: Update `docs/progreso.md`, delete tmp file, report, done.

**If CONFIRMED**: Proceed to Phase 4.

**If PARTIALLY-FIXED**: Document what works and what doesn't. The remaining
broken part is the actual gap to fix.

**Collateral findings**: Create entries in a `## Collateral` section at the
end of the test output. These become candidates for future `/hunt-gap` runs.
Do not fix them now.

---

## Phase 4 — Root cause analysis

### Step 1: Hypotheses (minimum 2)

```
Hypothesis A: [description]
  Evidence: [what supports this]
  How to verify: [specific code to read or experiment to run]

Hypothesis B: [description]
  Evidence: [what supports this]
  How to verify: [specific code to read or experiment to run]
```

### Step 2: Investigate

- **Read the code path** from entry point to failure. For wire-visible gaps:
  `handler.rs` → `executor/` → specific module.
- **Check all call sites** — a fix in one place often needs to be applied to
  similar patterns. In GAP-A.1, the BIGINT index bug required fixing 4
  separate encode points in `planner_select.rs`, not just one.
- **Use research/ for reference** — compare with PostgreSQL, MySQL, MariaDB,
  SQLite, DuckDB source code when the gap is about SQL semantics.

### Step 3: Document root cause

```
ROOT CAUSE:
  File: [path:line]
  Function: [name]
  Problem: [what exactly is wrong and why]
  Scope: [how many call sites / similar patterns need fixing]
```

---

## Phase 5 — Implement the fix

### Step 1: Declare effort

```
Effort required: [low | medium | high | max]
Reason: [one line]
```

### Step 2: Make the minimal fix

- Fix the **root cause**, not symptoms
- Fix **all call sites** that have the same pattern — don't leave partial fixes
- Do **NOT** refactor, clean up, or "improve" surrounding code
- Do **NOT** add features beyond what the gap describes
- For type coercion fixes: remember `axiomdb_types::coerce(value, target, mode)`
- For unwrap→error fixes: use the existing error type

### Step 3: Build and test incrementally

```bash
export PATH="$HOME/.rustup/toolchains/stable-x86_64-unknown-linux-gnu/bin:$PATH"

# 1. Build the affected crate
cargo build -p axiomdb-<crate>

# 2. Run unit tests for the crate
cargo test -p axiomdb-<crate>

# 3. If it touches SQL: rebuild server + re-run gap test
cargo build -p axiomdb-server
python3 tools/tmp_gap_test.py
```

### Step 4: Iterate if needed

If the test still fails after the fix:
1. Re-read the test output carefully
2. Re-examine hypotheses — was the root cause correct?
3. Check if the fix was applied to all call sites
4. Look for a second bug layered on top of the first
5. Do NOT add workarounds — find the real cause

---

## Phase 6 — Validate and close

### Step 1: Regression check

```bash
# Full wire test (must pass all existing tests)
python3 tools/wire-test.py

# Unit tests for affected crate
cargo test -p axiomdb-<crate>

# Clippy
cargo clippy -p axiomdb-<crate> -- -D warnings
```

If wire-test shows regressions: the fix broke something else. Investigate
before proceeding — do not skip regressions.

### Step 2: Clean up

```bash
rm tools/tmp_gap_test.py
```

### Step 3: Update documentation

In `docs/progreso.md`, change the gap entry:
```
- [x] GAP-X.Y description — closed: [brief explanation of root cause and fix]
```

### Step 4: Final report

```
GAP [id] CLOSED

Root cause: [one-line summary]
Fix: [file(s) changed and what was done]
Scope: [N files, M lines changed]
Tests: [what was verified — unit + wire + gap test]
Regressions: none / [list any and how they were resolved]
Collateral findings: [new gaps discovered during this hunt]
```

### Step 5: Commit (follow CLAUDE.md commit format)

```bash
git add [specific files]
git commit -m "fix(fase-N): close GAP-X.Y — [description]

- [detail 1]
- [detail 2]"
```

Do NOT include Co-Authored-By from Claude (per CLAUDE.md).

---

## Rules

1. **Never skip reproduction.** A gap that cannot be reproduced is not a gap.
2. **One gap at a time.** Do not batch multiple fixes.
3. **Always clean up.** Delete `tools/tmp_gap_test.py` after closing each gap.
4. **Stay focused.** New bugs found during a hunt go to collateral findings,
   not inline fixes — unless they block the current fix.
5. **Fix all call sites.** When a pattern is wrong in one place, grep for
   the same pattern everywhere. A partial fix is worse than no fix.
6. **Research reference code.** Use `research/postgresql/`, `research/mariadb/`,
   `research/sqlite/`, `research/duckdb/` for SQL semantics questions.
7. **Minimal fix.** Root cause only, nothing more.
8. **Test 3 ways.** Every Python test needs: control + gap test + edge case.
9. **Wire-test must not regress.** If it does, fix the regression before closing.
10. **Document collateral.** Report unexpected findings so they become future hunts.
