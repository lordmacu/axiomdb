# /brainstorm — Explore before proposing

Before proposing any solution, run this complete protocol:

## Step 1 — Read context
- Read `db.md` section for the current phase
- Read `docs/fase-anterior.md` if it exists
- Read codebase files relevant to the task
- Read `specs/fase-actual/` if it exists

## Step 2 — Ask the user questions
Do not propose anything until you have answers to:
- What is the exact expected behavior?
- Are there known edge cases I need to handle?
- Are there performance constraints (latency, throughput)?
- Are there compatibility constraints with previous phases?
- How much time do we have for this phase?

## Step 3 — Propose approaches with trade-offs
Always present **at least 2 options**, never just the "best" one:

```
Approach A: [name]
  Pros: [list]
  Cons: [list]
  When to choose it: [condition]

Approach B: [name]
  Pros: [list]
  Cons: [list]
  When to choose it: [condition]
```

## Step 4 — Sprint with dependencies
If the task has subtasks, write an explicit sprint:

```
Sprint: [phase name]
Estimate: [N hours/days]

├── Task 1: [name]
│   Description: [what it does]
│   Dependencies: none
│   Done criterion: [verifiable]
│
├── Task 2: [name]
│   Description: [what it does]
│   Dependencies: Task 1
│   Done criterion: [verifiable]
│
└── Task 3: [name]
    Description: [what it does]
    Dependencies: Task 1
    Done criterion: [verifiable]
```

## Expected output
At the end of the brainstorm, the user and Claude must have agreed on:
- [ ] Chosen approach and why
- [ ] Sprint with tasks and dependencies
- [ ] Clear done criteria
- [ ] Identified risks

Next step: `/spec-task` for the first task in the sprint.
