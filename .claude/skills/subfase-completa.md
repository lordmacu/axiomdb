# /subfase-completa — Mark a subphase as completed

Run this skill when finishing each individual subphase.
Updates progress, memory, and makes an automatic commit.

## Usage

```
/subfase-completa N.M
```

Example: `/subfase-completa 1.3` marks subphase 1.3 as completed.

## Process this skill executes

### Step 1 — Verify the subphase is truly ready

```bash
cd /Users/cristian/dbyo

# Tests for the affected crate
cargo test -p dbyo-CRATE --quiet 2>&1 | tail -5

# Clippy with no warnings
cargo clippy -p dbyo-CRATE -- -D warnings 2>&1 | head -10

# Correct format
cargo fmt --check 2>&1 | head -5
```

If any fails: **DO NOT mark as completed.** Fix it first.

### Step 2 — Mark in docs/progreso.md

Find the line for subphase N.M and change:
```
- [ ] N.M ⏳ description
```
to:
```
- [x] N.M ✅ description — completed YYYY-MM-DD
```

Also update the parent phase state if all its subphases are complete:
```
### Phase N — Name `⏳`   →   ### Phase N — Name `✅`
```

### Step 3 — Update statistics at the end of the file

Recalculate and update the statistics block:
```
Total subphases:  185
Completed:         X  (Y%)
In progress:       Z  (W%)
Pending:         ...

Current phase:     [next pending subphase]
Last completed: N.M — description — YYYY-MM-DD
```

### Step 4 — Update memory/project_state.md

```markdown
## In progress
- Subphase [N.M+1]: [description]

## Recently completed
- Subphase N.M: description (YYYY-MM-DD)
```

### Step 5 — If the full phase finished, update memory/architecture.md

Only if N.M was the last subphase of Phase N:
```markdown
## Implemented crates
- `dbyo-NAME` — description (Phase N completed YYYY-MM-DD)
```

### Step 6 — Commit

```bash
git add docs/progreso.md .claude/projects/*/memory/project_state.md
git commit -m "progress(N.M): complete [brief subphase description]

Subphase N.M of Phase N completed.
Progress: X/225 subphases (Y%)"
```

### Step 7 — Report to user

```
✅ Subphase N.M completed — [description]

Phase N progress: [X of Y subphases] ████░░░░ 60%
Total progress:   [X of 185]         ██░░░░░░  8%

Next subphase: N.M+1 — [description]
```

---

## Quick reference format

```
⏳ pending
🔄 in progress (mark manually if you start without finishing)
✅ completed
⏸ blocked (depends on something external)
```

To mark as in progress without completing:
```
- [ ] N.M 🔄 description — started YYYY-MM-DD
```

To mark as blocked:
```
- [ ] N.M ⏸ description — blocked by: [reason]
```
