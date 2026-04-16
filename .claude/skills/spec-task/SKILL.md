---
name: spec-task
description: Write a task specification into specs/fase-N/spec-nombre.md — scope, behavior, API, edge cases, done criteria, before implementing
---

# /spec-task — Write the spec for a task

Write a precise specification before coding. The spec is the contract
between brainstorm and implementation — once agreed, it is the source
of truth for `/plan-task` and implementation.

## Usage

```
/spec-task fase-N name
```

Example: `/spec-task 2 btree-insert` writes `specs/fase-02/spec-btree-insert.md`.

## Prerequisites

Before running this skill you should have finished `/brainstorm` and
agreed with the user on:

- Chosen approach and why
- Sprint with tasks and dependencies
- Clear done criteria
- Identified risks

If any of the above is missing, run `/brainstorm` first.

## Step 1 — Create the spec file

```bash
mkdir -p specs/fase-N
touch specs/fase-N/spec-<nombre>.md
```

Naming rule: kebab-case, no accents, no spaces. `spec-btree-insert.md`,
not `Spec BTree Insert.md`.

## Step 2 — Use this template

```markdown
# Spec: [task name]

Phase: N — [phase name]
Task: [task name within the sprint]
Status: draft | approved | implemented

## Context

[Two or three sentences. What part of the system does this touch?
What came before it, what comes after? Link to the phase in db.md.]

## Goal

[One sentence. What this task delivers — not how.]

## Non-goals

[Explicit out-of-scope items. Prevents scope creep during implementation.]
- Not handling [X] — deferred to Phase [M]
- Not optimizing [Y] — benchmark-only concern

## Behavior

### Public API

\`\`\`rust
// Exact signatures, types, traits, errors
pub trait Example {
    fn operation(&self, arg: Type) -> Result<Output, DbError>;
}
\`\`\`

### Semantics

[Precise description of what each operation does.
Include preconditions, postconditions, invariants.]

- Precondition: [what must be true before calling]
- Postcondition: [what is guaranteed after returning Ok]
- Invariant: [what remains true across calls]

### Error cases

| Input | Expected error | Message |
|-------|----------------|---------|
| [case] | `DbError::X` | `"..."` |
| [case] | `DbError::Y` | `"..."` |

## Edge cases

Explicit list of corner cases that MUST be handled:

- [ ] Empty input
- [ ] Maximum-size input
- [ ] Concurrent access (if applicable)
- [ ] Crash mid-operation (if applicable)
- [ ] Unicode / non-ASCII (if applicable)
- [ ] NULL / None (if applicable)

Each one becomes a test case in `/plan-task`.

## On-disk format (if applicable)

\`\`\`
Byte layout:
  offset  size  field      description
  0       8     magic      0xAXIOMDB_...
  8       4     version    u32, current = 1
  12      N     payload    ...
\`\`\`

Compatibility rule: [can this format evolve? how?]

## Performance budget

| Operation | Target | Max acceptable |
|-----------|--------|----------------|
| [op]      | [X/s]  | [Y/s]          |

Reference: [related numbers in bench skill or CLAUDE.md].

## Dependencies

- Depends on: [other specs/crates]
- Blocks: [what cannot proceed until this is done]

## Open questions

[Things that still need to be decided. Each one must be resolved
before status moves from draft → approved.]

- [ ] Should [X] be [A] or [B]?
- [ ] What happens when [edge case]?

## Done criteria

[Verifiable, binary. No subjective language.]

- [ ] Public API matches the signatures above
- [ ] All edge cases have a test
- [ ] cargo test -p axiomdb-CRATE passes
- [ ] cargo clippy -p axiomdb-CRATE -- -D warnings passes
- [ ] Benchmark shows [operation] within budget
- [ ] Documentation: rustdoc on every public item
- [ ] [Any task-specific criterion]

## References

- Previous phase doc: `docs/fase-(N-1).md`
- Related spec: `specs/fase-N/spec-[other].md`
- External: [PostgreSQL src, MySQL manual, SQL standard section]
```

## Step 3 — Review with the user

Do not proceed to `/plan-task` until:

- [ ] User has read the spec
- [ ] Open questions are resolved
- [ ] Done criteria are accepted
- [ ] Status changed from `draft` to `approved`

## Step 4 — Commit the spec

```bash
git add specs/fase-N/spec-<nombre>.md
git commit -m "spec(fase-N): approve spec for [task name]

Spec approved. Next: /plan-task fase-N <nombre>"
```

## Rules

1. **Specs are for behavior, not implementation.** How goes in the plan.
2. **Be precise about types and errors.** Ambiguity here costs time later.
3. **Every edge case = a future test.** If it's not listed, it won't be tested.
4. **Resolve open questions before approving.** Draft specs are for thinking;
   approved specs are contracts.
5. **One task = one spec file.** Do not bundle.

Next step: `/plan-task fase-N <nombre>` to break the approved spec into
bite-sized implementation steps.
