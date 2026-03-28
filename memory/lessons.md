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

## 2026-03-26 - Large Rust module splits can be staged safely

- When a monolithic Rust module has too many private cross-dependencies, a directory module plus `include!` can be a good first refactor step.
- The goal of the first split is file-level responsibility and reviewability, not perfect visibility boundaries on day one.
- Keep the public facade stable first (`mod.rs` with the old exported functions), then tighten internal visibility in later cleanup if needed.
- This avoids mixing a readability refactor with accidental behavior changes.

## 2026-03-26 - Separate transport lifecycle from SQL session state

- Connection timeout policy, keepalive, and transport phase transitions should not live inside the SQL session object.
- Keep a dedicated runtime/lifecycle struct for wire-level state and let the session object stay focused on SQL variables, prepared statements, warnings, and counters.
- `COM_RESET_CONNECTION` is a good boundary test: it should reset session state without erasing transport metadata that came from the handshake.

## 2026-03-26 - DML performance needs two separate diagnoses

- A slow `DELETE` or `UPDATE` can bottleneck in candidate discovery or in index maintenance; do not treat them as one problem by default.
- `6.3b` fixed candidate discovery for `DELETE ... WHERE`; `5.19` only became obvious after re-measuring and isolating the remaining per-row `delete_in(...)` loop.
- For DML optimizations, keep one test layer for planner/executor semantics and another for direct tree/storage primitives. One without the other leaves too much room for false confidence.

## 2026-03-26 - Changed columns are not enough to skip UPDATE index work

- In AxiomDB, “the SET clause did not touch indexed columns” is not by itself a safe reason to skip index maintenance.
- If the heap update changes the `RecordId`, every index entry that stores that RID is logically stale even when the key bytes are identical.
- The safe rule is stronger:
  - skip an index only if the RID stayed stable
  - and the logical key / predicate membership for that index stayed the same
- This is the same structural reason PostgreSQL HOT can skip index writes and the old AxiomDB path could not.

## 2026-03-26 - Use incremental validation during implementation

- Do not run `cargo test --workspace` after every edit or every short implementation cycle.
- During implementation, start with `cargo test -p <touched-crate>` and expand only to directly affected dependent crates.
- Expand the validation scope when the change touches public APIs, shared crates, on-disk format, WAL/recovery, SQL semantics, or MySQL wire-visible behavior.
- Run `tools/wire-test.py` only when the change is observable through the MySQL protocol.
- Keep `cargo test --workspace`, `cargo clippy --workspace -- -D warnings`, and `cargo fmt --check` as the final close/review gate, not the inner development loop.

## 2026-03-27 - Barrier flushes must happen before the next statement savepoint

- If a feature buffers writes across statements, the flush decision belongs at the
  statement boundary, not deep inside the next statement handler.
- `5.21` exposed the failure mode clearly: a table-switch flush performed inside
  the next `INSERT` happened after the statement savepoint, so a duplicate-key
  error rolled back rows that logically belonged to earlier successful INSERTs.
- The safe rule is:
  - determine whether the current statement can continue the batch
  - if not, flush the batch
  - only then capture the savepoint for the current statement
- This is especially important for MySQL-visible modes like
  `rollback_statement`, `savepoint`, and `ignore`, where statement boundaries are
  part of the user-visible contract.

## 2026-03-27 - Benchmarks should be targeted, not mandatory on every close

- Do not run full benchmark suites by default for every subphase close.
- Run benchmarks when the subphase is performance-facing, changes a hot path, or
  could plausibly regress a previously measured critical workload.
- For non-performance subphases, prefer targeted tests, clippy, fmt, and wire
  smoke (if user-visible) instead of paying benchmark time with low signal.
- When a benchmark is run, keep it narrow and workload-specific:
  - use the scenario that motivated the subphase
  - compare against the latest relevant local baseline or competitor numbers
  - avoid rerunning unrelated scenarios unless the blast radius justifies it

## 2026-03-27 - Indexed UPDATE has two separate costs too

- Fixing indexed `UPDATE ... WHERE` discovery does not automatically fix update
  throughput end-to-end.
- `6.17` removed the planner-side full scan for PK/range/equality predicates,
  but the post-fix benchmark still showed a large gap.
- That measurement is useful: it means the remaining cost sits in the apply
  path after candidates are found, not in discovery anymore.
- Keep future UPDATE performance work split the same way:
  - candidate discovery
  - heap rewrite / stable-RID hit rate
  - index maintenance / root persistence

## 2026-03-27 - Parse shared config once, validate semantics at the edge

- For URI/DSN-like inputs, keep one shared parser that preserves information and
  returns a typed normalized shape.
- Do not let each consumer re-parse strings differently; that creates drift.
- The parser should own syntax and ambiguity rejection.
- Each consumer should own only semantic validation of the subset it actually
  supports.
- This is especially important when aliases exist:
  - `mysql://` and `postgres://` can be valid parse aliases
  - but that must not silently imply protocol support the product does not have

## 2026-03-27 - Direct storage rebuilds need a durability barrier before root rotation

- If a repair path writes new storage pages outside WAL and then swaps a catalog
  root to point at them, the new pages must be durable before the root swap is
  committed.
- The safe ordering is:
  - build new pages
  - `storage.flush()`
  - commit the catalog/root metadata change
- Otherwise recovery can replay the metadata/root change and make the new root
  visible while the rebuilt pages were only ever resident in memory.
- This applies to index rebuild / root-rotation paths in particular, not only to
  startup repair code.

## 2026-03-27 - Shared batch apply helpers do not imply shared uniqueness shortcuts

- If two INSERT paths both end in the same grouped heap/index write code, that
  does not mean they can also share the same uniqueness fast path.
- `5.21` staging can safely use a `committed_empty` shortcut only because it
  prevalidates duplicate keys across the staged batch with `unique_seen`.
- `6.18` exposed the boundary clearly: the immediate multi-row `VALUES` path can
  reuse grouped physical apply, but must keep `skip_unique_check = false` and
  reject same-statement duplicate PRIMARY KEY / UNIQUE values before any partial
  batch becomes visible.

## 2026-03-27 - Timer-based batching and leader-based piggybacking solve different fsync problems

- The old timer-based group commit from `3.19` helps when many connections are
  already waiting together.
- It does **not** solve the common single-connection autocommit case, because
  there is no overlap unless the next commit can piggyback on an in-flight fsync.
- `6.19` therefore needed a different primitive: leader election plus
  `flushed_lsn` / `pending_lsn` tracking, inspired by MariaDB
  `group_commit_lock`.
- The lesson is to benchmark the real arrival pattern, not just "concurrency" in
  the abstract. A batching primitive that is perfect for many waiters can still
  be the wrong primitive for pipelined single-client commits.
- A second lesson from the closure attempt: a leader-based fsync pipeline still
  does not improve a **sequential** MySQL wire client if the handler waits for
  durability before it sends the OK packet.
- Under request/response autocommit, the next statement never reaches the server
  while the current fsync is in flight, so there is nothing to piggyback.

## 2026-03-27 - `USE` is not real multi-database support unless the catalog owns it

- A wire-level `USE db` implementation is not enough by itself.
- Before `22b.3a`, the server could remember a selected database name in session
  state, but that name did not change table ownership, `SHOW DATABASES`, or DDL.
- The correct boundary is:
  - databases must exist in the catalog
  - table ownership must be persisted separately from the session
  - analyzer resolution must use the selected database when binding names
- Keeping database ownership outside `TableDef.schema_name` avoided a noisier
  on-disk migration and kept room for future schema support inside each
  database.

## 2026-03-27 - Handshake validation must fail before the final OK packet

- MySQL clients that connect with `CLIENT_CONNECT_WITH_DB` expect unknown
  databases to fail the handshake itself, not to succeed auth and then fail on
  the first command.
- `22b.3a` exposed a subtle wire bug: validating the catalog after sending auth
  success produced the wrong observable protocol behavior.
- The safe rule is:
  - authenticate credentials
  - validate the requested default database
  - only then send the final OK packet
- This is a good reminder that wire-visible state transitions are part of the
  contract, not just internal implementation detail.
