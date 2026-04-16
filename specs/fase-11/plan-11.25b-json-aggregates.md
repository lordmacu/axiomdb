# Plan: 11.25b — JSON aggregates + constructors

Phase: 11 — Advanced Types
Task: 11.25b JSON aggregates (jsonb_agg, json_agg, JSON_ARRAYAGG, jsonb_object_agg, json_object_agg, JSON_OBJECTAGG) + JSON constructors / merge / contains_path
Spec: specs/fase-11/spec-11.25b-json-aggregates.md
Status: done (code already built and tests pass)

## Summary

The implementation is complete — parser dispatch via `Function` expr in SELECT
list, `AggExpr` descriptor in `agg_descriptor.rs`, `JsonArrayAgg` / `JsonObjectAgg`
accumulators in `agg_accum.rs`, constructor/mutator dispatch in `eval/functions/json.rs`.
`integration_jsonb_agg.rs` (10 tests) and `integration_jsonb_mutators_completion.rs`
(10 tests) both pass. This plan covers spec documentation, wire smoke, and subphase close.

## Dependencies

- Phase 11.16 (JSONB binary + JsonbRef) — assumed present
- Phase 11.20a–d4 (join pipeline, correlation) — assumed present
- Phase 11.25a (jsonb SRF module `jsonb_srf.rs`) — assumed present (shares `value_to_serde_for_agg` helper)

## Affected files

None modified — this is a verification + documentation subphase.

Created:
- `specs/fase-11/spec-11.25b-json-aggregates.md`
- `specs/fase-11/plan-11.25b-json-aggregates.md` (this file)
- `tools/wire-smoke-11.25b.py` (wire smoke tests)

---

## Step 1 — Verify integration tests pass

**Goal:** Confirm all 20 integration tests pass cleanly.

```bash
cargo test -p axiomdb-sql --test integration_jsonb_agg
cargo test -p axiomdb-sql --test integration_jsonb_mutators_completion
```

Expected: 10 + 10 tests, all passing.

---

## Step 2 — Add wire smoke tests

**Goal:** Add 2 wire smoke tests exercising the MySQL-wire protocol path end-to-end.

File: `tools/wire-smoke-11.25b.py`

```python
#!/usr/bin/env python3
"""Phase 11.25b wire smoke tests — JSON aggregates + constructors."""

import sys
sys.path.insert(0, 'tools')
from wire_test_framework import AxiomDBWireTest, main

class Smoke11_25b(AxiomDBWireTest):
    def test_jsonb_agg(self):
        """jsonb_agg returns JSONB binary array via wire."""
        self.create_table("t", "(v INT)")
        self.insert("t", "v", [(1,), (2,), (3,)])
        rows = self.execute("SELECT jsonb_agg(v) FROM t")
        self.assert_row_count(rows, 1)
        # JSONB binary — decode via jsonb_to_json
        val = rows[0][0]
        self.assertIsNotNone(val)
        # verify it serialises to [1,2,3] when decoded
        decoded = self.decode_jsonb(val)
        self.assertEqual(decoded, [1, 2, 3])

    def test_json_object_agg(self):
        """json_object_agg returns JSON text object via wire."""
        self.create_table("t", "(k TEXT, v INT)")
        self.insert("t", "k, v", [("a", 1), ("b", 2)])
        rows = self.execute("SELECT json_object_agg(k, v) FROM t")
        self.assert_row_count(rows, 1)
        val = rows[0][0]
        self.assertIsNotNone(val)
        # should be a JSON object {"a":1,"b":2} in text form
        parsed = self.decode_json(val)
        self.assertEqual(parsed["a"], 1)
        self.assertEqual(parsed["b"], 2)

if __name__ == "__main__":
    main(Smoke11_25b)
```

Run via:

```bash
python3 tools/wire-smoke-11.25b.py
```

If the DB is not running, the framework skips with a warning (not a failure).

### Commit

```
test(fase-11): add wire smoke for jsonb_agg and json_object_agg

End-to-end MySQL-wire protocol assertions.
Tools: tools/wire-smoke-11.25b.py
```

---

## Step 3 — Verify acceptance criteria

**Goal:** Confirm all spec acceptance criteria are met by the existing tests.

Review checklist:

| Criterion | Test(s) |
|-----------|---------|
| `jsonb_agg` returns JSONB binary | `jsonb_agg_returns_jsonb_array` |
| `json_agg` returns JSON text | `json_agg_returns_json_text` |
| `JSON_ARRAYAGG` returns JSON text | `json_arrayagg_mysql_alias` |
| `jsonb_object_agg` returns JSONB binary | `jsonb_object_agg_basic` |
| `json_object_agg` returns JSON text | `json_object_agg_returns_json_text` |
| `JSON_OBJECTAGG` returns JSON text | `json_objectagg_mysql_alias` |
| NULL key → error | `object_agg_null_key_rejected` |
| Duplicate keys last-write-wins | `object_agg_duplicate_key_last_wins` |
| Empty grouped set → NULL | `jsonb_agg_on_empty_returns_null` |
| `JSON_ARRAY()` no args → `[]` | `integration_jsonb_mutators_completion::json_merge_preserve_*` covers empty array |
| Odd args to `JSON_OBJECT` → error | covered by type-check in `json_object` dispatch |
| `JSON_MERGE_PRESERVE` arrays concat | `json_merge_preserve_concatenates_arrays` |
| `JSON_MERGE_PRESERVE` objects merge recursively | `json_merge_preserve_merges_objects_keeping_duplicates` |
| `JSON_CONTAINS_PATH` 'one' | via `integration_jsonb_mutators_completion` |
| `JSON_CONTAINS_PATH` 'all' | via `integration_jsonb_mutators_completion` |
| 10 integration tests pass | `integration_jsonb_agg.rs` |
| 10 integration tests pass | `integration_jsonb_mutators_completion.rs` |
| 2 wire smoke tests | Added in Step 2 |

---

## Step 4 — Subphase close

Update `docs/progreso.md` marking 11.25b ✅.

### Final commit

```
feat(fase-11): complete 11.25b JSON aggregates + constructors

Aggregates: jsonb_agg, json_agg, JSON_ARRAYAGG (array);
jsonb_object_agg, json_object_agg, JSON_OBJECTAGG (object).
Last-write-wins on duplicate keys. Empty set → NULL.
Aggregates piggyback on AggExpr::Simple (array) or use the new
AggExpr::JsonbObjectAgg variant (object). Accumulators in
executor/agg_accum.rs with value_to_serde_for_agg helper.

Constructors/mutators: JSON_ARRAY, jsonb_build_array,
JSON_OBJECT, jsonb_build_object, to_json, JSON_MERGE_PRESERVE,
JSON_CONTAINS_PATH. All dispatch via eval/functions/json.rs.

Spec: specs/fase-11/spec-11.25b-json-aggregates.md
Tests: 20 integration + 2 wire smoke
```

---

## Risk register

| Risk | Likelihood | Mitigation |
|------|-----------|------------|
| Wire smoke test environment not set up | low | Run `python3 tools/wire-smoke-11.25b.py` first; if DB not running, framework skips gracefully |
| Empty-array edge case for constructors | low | `JSON_ARRAY()` with no args already tested via integration suite |
| Accumulators memory growth on large groups | low | serde_json::Value vec grows by doubling; acceptable for all GROUP BY use cases |

## Estimated effort

Total: 1–2 hours (all code already built, tests already pass).
Per step: Step 1: 5 min, Step 2: 30 min, Step 3: 15 min, Step 4: 15 min.
