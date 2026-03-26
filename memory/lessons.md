# Lessons Learned

## 2026-03-25 - Spec workflow discipline

- Read the relevant codebase files before writing a single line of the spec, not after. Good reasoning after the fact does not repair a spec as cleanly as reading first.
- Every file named in `Dependencies` must have been read before the spec exists. If a dependency is only conceptual and not tied to a real file, the spec is too abstract.
- Every "reuse X" claim requires reading `X` first. Do not assume an existing helper or path is correct just because it sounds reusable.
- Specs must list explicitly which files were reviewed before writing them. This is a process signal, not decoration.
- Acceptance criteria must distinguish ambiguous interpretations, not only cover the happy path.
- When a feature has multiple plausible semantics, add criteria that make the wrong interpretation unshippable.

## 2026-03-25 - Concrete checklist for future specs

- Before `/spec-task`, list the real files to read.
- In the spec, include a section equivalent to: "These files were reviewed before writing this spec".
- In `Dependencies`, mention real codebase files, not only modules or concepts.
- If reusing an existing implementation path, read that implementation first and record whether only the shape is reused or also the behavior.
- Add acceptance criteria for both sides of every ambiguity:
  - default scope vs explicit scope
  - exact wildcard semantics vs simplified substring matching
  - lock-free snapshot path vs mutex-bound execution path
- If those checks are missing, treat the spec as suspect and re-read the code before continuing.

## 2026-03-25 - No loose ends before closing brainstorm/spec/plan

- `/brainstorm`, `/spec-task` and `/plan-task` are not done if they still leave unresolved design choices for the implementer to decide mid-coding.
- If the plan says "acceptable shapes", "one option is", or leaves multiple API signatures open, it is incomplete and must be resolved before calling the phase finished.
- Open points such as return types, error propagation paths, exact call sites, and ownership boundaries must be decided in the plan itself.
- The goal is zero loose ends: implementation should follow the plan, not complete it.

## 2026-03-25 - Research citations in specs and plans

- When `research/` influences a brainstorm, spec, or plan, cite the exact file path that inspired the decision.
- Do not write only "PostgreSQL", "DuckDB", or "MariaDB". Name the concrete source file and what idea came from it.
- Prefer a short section such as `Research synthesis`, `What we borrow`, or `Research citations` so the reader can trace the reasoning without reopening the whole session.
- If a design choice was inspired by one file and constrained by AxiomDB's current code, say both explicitly.
- AxiomDB comes first: before citing `research/`, name the AxiomDB file(s) that constrain the design. External inspiration never overrides the current codebase silently.
- For each research citation, state the role of the source: compatibility behavior, algorithm, data structure, testing oracle, or anti-pattern to avoid.
- Use an explicit `borrow / reject / adapt` mindset:
  - what was borrowed
  - what was rejected
  - how AxiomDB adapts it
- Do not cargo-cult whole architectures. Borrow techniques, not entire systems, unless the plan explicitly says a larger refactor is intended.
- If a behavior decision comes from research, reflect it in at least one acceptance criterion or one concrete test plan item.
- Every “similar to X” or “like Y” claim needs a real file path from `research/` plus the exact idea being referenced.
- If research suggests something better than the current codebase can support, say so explicitly and document the tension:
  - inspiration from research
  - implementation limited by AxiomDB's current architecture
- When possible, tie each external inspiration to the local verification path that will prove it in AxiomDB: unit test, integration test, wire test, or benchmark.

## 2026-03-26 - Critical subphases require full research synthesis

- When a subphase can affect correctness, durability, transactional rollback, index consistency, or crash recovery, treat it as research-critical by default.
- For research-critical subphases, review the relevant AxiomDB files first and then read all relevant engines under `research/`, not only the most obvious reference system.
- The spec and plan must include a clear synthesis of:
  - what each engine contributed
  - which ideas were rejected
  - why the AxiomDB adaptation is safer or better for the current architecture
- Do not optimize only for speed in a critical subphase. Preserve invariants first, then derive the fast path from a correctness-safe design.
- If one approach is faster but weakens rollback, FK correctness, index consistency, or recovery guarantees, reject it unless the spec explicitly opens a follow-up phase to recover those guarantees.
