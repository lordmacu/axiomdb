# Project State

## 2026-03-26

- Phase 5 subphase `5.11b` is closed in code, tests, and docs.
- Phase 5 subphase `5.11c` is closed in code, tests, and docs.
- Phase 5 subphase `5.19` is closed in code, tests, and docs.
- Phase 5 subphase `5.19a` is closed in code, tests, and docs.
- Phase 5 subphase `5.19b` is closed in code, tests, and docs.
- Phase 5 subphase `5.20` is closed in code, tests, and docs.
- `COM_STMT_SEND_LONG_DATA` was already largely implemented in the network layer; the remaining work was closure:
  - wire smoke coverage
  - protocol-facing tests
  - tracker reconciliation
  - documentation alignment
- The SQL executor is now split under `crates/axiomdb-sql/src/executor/` with a stable `mod.rs` facade.
- The refactor was structural only: `execute`, `execute_with_ctx`, and `last_insert_id_value()` kept the same paths and behavior.
- The expression evaluator now lives under `crates/axiomdb-sql/src/eval/` with `mod.rs`
  preserving the old public API while internals are split across `context.rs`,
  `core.rs`, `ops.rs`, and `functions/`.
- `DELETE ... WHERE` and the old-key half of `UPDATE` now batch-delete exact encoded keys per index through `BTree::delete_many_in(...)`.
- The hot path moved from `N` root descents per statement to one ordered delete batch per affected index.
- UPDATE now has a stable-RID fast path:
  - same-slot heap rewrites preserve `(page_id, slot_id)` when the new row still fits
  - WAL records that branch as `EntryType::UpdateInPlace`
  - unchanged indexes are skipped only when RID stability makes that safe
- Latest 50K-row local benchmark snapshot:
  - `UPDATE ... WHERE active = TRUE`: `648K rows/s`
  - `DELETE WHERE id > 25000`: `1.13M rows/s`
- Validation workflow was tightened:
  - iterative development uses targeted crate tests plus related dependents only when the blast radius justifies it
  - `cargo test --workspace` remains mandatory, but only as the subphase/phase closing gate
  - `tools/wire-test.py` is part of the loop only for MySQL wire-visible changes
- Remaining notable Phase 5 items after this close:
  - `5.15` DSN parsing
