# Spec: 11.18c — JSONB path operators follow-up

## Status

Implemented.

`#>`, `#>>`, and `#-` already exist in the repo, but `11.18c` was never
closed cleanly as its own subphase: there is no dedicated spec/plan pair,
`docs/progreso.md` still carries stale wording, and the documented dependency
on native `TEXT[]` no longer matches the implementation that already accepts a
JSONB string-array RHS.

This subphase defines the bounded contract that AxiomDB will actually support
for `11.18c` and the gates required to close it.

## What to build

Close `11.18c` as **JSONB path operators with JSONB-array RHS**, not as native
PostgreSQL `text[]` parity.

Supported operators:

| Operator | Signature in AxiomDB | Meaning |
|---|---|---|
| `#>` | `jsonb #> jsonb -> jsonb` | Extract nested JSONB value by path |
| `#>>` | `jsonb #>> jsonb -> text` | Extract nested value rendered as text |
| `#-` | `jsonb #- jsonb -> jsonb` | Delete nested path from document |

RHS contract:

- AxiomDB accepts a **JSONB array path** such as
  `CAST('["a","b",1]' AS JSONB)`.
- String array members navigate object keys.
- Integer array members navigate array indexes.
- This is a **documented PostgreSQL divergence**: PostgreSQL accepts `text[]`.

## Expected behavior

### Path extraction

- `doc #> path` returns the nested JSONB subtree/value at `path`.
- Missing path returns `NULL`.
- `NULL` on either side returns `NULL`.

### Text extraction

- `doc #>> path` returns the located value rendered as SQL text.
- JSON strings lose their outer quotes.
- Numeric / boolean / null JSON scalars render the same way they do through
  existing JSON/JSONB text extraction paths.
- Missing path returns `NULL`.
- `NULL` on either side returns `NULL`.

### Path delete

- `doc #- path` removes the targeted key / array element / nested subtree.
- Missing path is a no-op and returns the original document.
- Empty path returns the original document unchanged.
- `NULL` on either side returns `NULL`.
- Scalar-root delete errors remain aligned with the current JSONB mutator rules.

### Lexer / parser compatibility

- `#>`, `#>>`, and `#-` must tokenize before generic `#` comment stripping.
- `#` line comments are only stripped when they begin a line after optional
  whitespace; embedded operator uses must remain intact.
- JSON string literals containing `#` must remain unaffected.

## Acceptance criteria

- [x] Dedicated `11.18c` spec and plan exist.
- [x] `docs/progreso.md` describes `11.18c` as **JSONB-array RHS divergence**,
      not as a `TEXT[]` blocker.
- [x] `memory/project_state.md` no longer lists `11.18c` as pending after
      closure.
- [x] `docs/fase-11.md` records the closeout for `11.18c`.
- [x] `tests/integration_jsonb_path_ops.rs` covers:
  - object extraction
  - array index extraction
  - text rendering
  - nested delete
  - array delete
  - missing-path semantics
  - NULL propagation
- [x] Wire smoke includes a bounded `11.18c` block for `#>` / `#>>` / `#-`.
- [x] `cargo fmt --check` passes.
- [x] `cargo test -p axiomdb-sql --test integration_jsonb_path_ops` passes.
- [x] `python3 tools/wire-test.py` passes.
- [x] `cargo test --workspace` passes.
- [x] `cargo clippy --workspace -- -D warnings` passes.

## Out of scope

- Native SQL `TEXT[]` type.
- PostgreSQL-perfect `text[]` RHS parity.
- Additional JSONB operators outside `#>`, `#>>`, `#-`.
- Planner pushdown work from `11.21h`.
- JSONB mutator redesign beyond current `#-` semantics.

## Approach decision

### Approach A — close the existing JSONB-array implementation

Pros:
- Matches the actual code already shipped in the repo.
- Low-risk: mostly validation, smoke coverage, and documentation alignment.
- Keeps `11.18c` bounded and unblocks the remaining Phase 11 follow-ups.

Cons:
- Preserves a documented PostgreSQL divergence.
- Leaves native `TEXT[]` for a future array phase.

### Approach B — re-open `11.18c` and require native `TEXT[]` parity

Pros:
- Cleaner PostgreSQL surface.
- Avoids the JSONB-array RHS divergence.

Cons:
- Wrong cut for this point in the roadmap.
- Pulls array type-system work into a JSONB closeout.
- Risks destabilizing already-passing operator behavior.

Chosen: **Approach A**.
