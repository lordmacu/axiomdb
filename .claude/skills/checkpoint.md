# /checkpoint — Save context when pausing

When a task must be paused mid-way, save the exact state
so the next session can continue without questions.

## Create checkpoint

```bash
DATE=$(date +%Y%m%d-%H%M)
cat > /Users/cristian/nexusdb/docs/checkpoint-$DATE.md << 'CHECKPOINT'
# Checkpoint [DATE]

## Phase state
Phase: N — [name]
Current task: [name of the task within the sprint]

## What was being done exactly
[precise description — which function, which file, which problem]

## Pending decision (if any)
[what needed to be decided before continuing]
[options considered]
[missing information to decide]

## Exact next step
[precise instruction — enough to continue without context]
Example: "Implement fn insert() in crates/axiomdb-index/src/btree.rs line 42,
         following the algorithm in the spec at specs/fase-02/spec-btree.md section 'Insert'"

## Test state
cargo test result: [X passing, Y failing]
Failing tests (expected during development):
- [test name] — [why it fails, what is missing]

## Modified files
- [file] — [what changed]
- [file] — [what changed]

## What NOT to redo
[things already tried that did not work]
CHECKPOINT

git add docs/checkpoint-$DATE.md
git commit -m "checkpoint: pause at [brief description]"
```

## When starting a session with a checkpoint

```
1. Read the most recent docs/checkpoint-DATE.md
2. Run targeted tests for the touched crate(s) and related dependents to see the current state
3. Reserve cargo test --workspace for the final close/review gate
4. Read the "Exact next step" and execute it
5. Delete the checkpoint when the task is complete
```

## When to create a checkpoint

- At the end of a session without having completed the phase
- Before a long pause (> 2 hours)
- When the conversation context is nearly full
- When there is a pending decision that requires external input
