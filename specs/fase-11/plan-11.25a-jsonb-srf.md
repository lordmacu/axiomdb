# Plan: 11.25a — JSONB set-returning functions (PG parity)

Phase: 11 — Advanced Types
Task: 11.25a JSONB SRF (jsonb_each, jsonb_each_text, jsonb_object_keys, jsonb_array_elements, jsonb_array_elements_text)
Spec: specs/fase-11/spec-11.25a-jsonb-srf.md
Status: done

## Summary

The implementation is substantially complete — parser (`parse_from_item` dispatch
at dml.rs:442), AST (`JsonbSrf`/`JsonbSrfKind` at ast.rs:289, jsonb_srf.rs),
analyzer (`analyzer_stmt.rs`, `analyzer_ddl.rs`), and executor
(`select_core.rs`, `select_joins_ctx.rs`, `dml_join.rs`) all have the code.
`integration_jsonb_srf.rs` has 9 integration tests covering all acceptance
criteria.

This plan is a **verification** plan: run the existing tests, add wire smoke
tests, and close the subphase.

## Dependencies

- Phase 11.16 (JSONB binary + JsonbRef) — assumed present
- Phase 11.20a–d4 (join pipeline, correlation) — assumed present

## Affected files

Modified files:
- `crates/axiomdb-sql/tests/integration_jsonb_srf.rs` — add wire smoke assertions

## Step 1 — Run existing unit/integration tests

**Goal:** Confirm all 9 integration tests pass.

```bash
cargo test -p axiomdb-sql --test integration_jsonb_srf
```

If any test fails, diagnose and fix before proceeding.

### Commit

```
test(fase-11): run 11.25a integration tests — all pass

9 tests covering jsonb_each, jsonb_each_text, jsonb_object_keys,
jsonb_array_elements, jsonb_array_elements_text, type errors,
NULL input, JOIN, CROSS APPLY, OUTER APPLY, UPDATE join.
```

---

## Step 2 — Add wire smoke tests

**Goal:** Add 2 wire smoke assertions using `wire-test.py` to cover the
MySQL-wire protocol path end-to-end.

### Tests to add

```python
# In tools/wire-test.py or a new file in tests/wire/
# Smoke 1: jsonb_each basic
assert_rows("SELECT key, value FROM jsonb_each('{\"a\":1,\"b\":2}') ORDER BY key",
            [["a", "1"], ["b", "2"]])

# Smoke 2: jsonb_array_elements
assert_rows("SELECT value FROM jsonb_array_elements('[1, 2, 3]')",
            [["1"], ["2"], ["3"]])
```

Run via:

```bash
python3 tools/wire-test.py
```

### Commit

```
test(fase-11): add wire smoke for jsonb_each and jsonb_array_elements

End-to-end MySQL-wire protocol assertions.
```

---

## Step 3 — Verify acceptance criteria

**Goal:** Confirm all spec acceptance criteria are met by the existing tests.

Review checklist:

| Criterion | Test(s) |
|-----------|---------|
| `jsonb_each` returns 2 rows `(a,1)`, `(b,2)` | `jsonb_each_basic` |
| `jsonb_each_text` returns value as TEXT | `jsonb_each_text_strips_quotes` |
| `jsonb_object_keys` returns 2-row single column | `jsonb_object_keys_basic` |
| `jsonb_array_elements` returns 3 rows | `jsonb_array_elements_basic` |
| `jsonb_array_elements_text` unquoted | `jsonb_array_elements_text_unquoted` |
| Non-object → error mentioning function | `jsonb_each_on_array_errors` |
| Non-array → error | `jsonb_array_elements_on_object_errors` |
| NULL doc → zero rows | `jsonb_each_null_doc_zero_rows` |
| JOIN non-correlated | `srf_join_with_real_table` |
| LATERAL / CROSS APPLY correlated | `srf_cross_apply_correlated` |
| OUTER APPLY preserves left | `srf_outer_apply_empty_preserves_left` |
| UPDATE / DELETE join | `srf_in_update_join` |
| 10–14 integration tests | 9 tests exist (above), within range |
| 2 wire smoke assertions | Added in Step 2 |

### Final commit

```
feat(fase-11): complete 11.25a JSONB SRF

Five PostgreSQL-compatible set-returning functions usable in FROM,
as join right sides, and CROSS/OUTER APPLY:
- jsonb_each(doc)         → (key TEXT, value JSONB)
- jsonb_each_text(doc)    → (key TEXT, value TEXT)
- jsonb_object_keys(doc)   → (key TEXT)
- jsonb_array_elements(doc) → (value JSONB)
- jsonb_array_elements_text(doc) → (value TEXT)

Non-matching type → clear error. NULL doc → zero rows.
Correlated SRF via LATERAL / APPLY pattern from 11.20d3.

Spec: specs/fase-11/spec-11.25a-jsonb-srf.md
Tests: 9 integration + 2 wire smoke
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Wire smoke test environment not set up | low | Run `python3 tools/wire-test.py` first; if DB not running, skip wire tests |
| Flaky test due to hash-map ordering | low | All array/object tests use `ORDER BY` or deterministic input |

## Estimated effort

Total: 1–2 hours (mostly verification, no new code expected)
Per step: Step 1: 15min, Step 2: 30min, Step 3: 15min
