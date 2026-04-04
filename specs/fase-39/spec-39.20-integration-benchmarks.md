# Spec: 39.20 — Integration Tests & Benchmarks

## What to build

End-to-end validation that the entire clustered index system (39.1-39.19) works
correctly and delivers the expected performance improvements. This is the final
gate before declaring Phase 39 complete.

## Test scenarios (minimum 10 tests)

### CRUD Lifecycle
1. CREATE clustered table → INSERT 1000 rows → SELECT all → verify count and order
2. INSERT → UPDATE non-key → SELECT → verify updated values
3. INSERT → DELETE → SELECT → verify deleted rows gone
4. INSERT → UPDATE → DELETE → VACUUM → verify space reclaimed
5. INSERT with secondary index → lookup via secondary → correct PK bookmark

### Transaction Safety
6. BEGIN → INSERT → ROLLBACK → SELECT → row not visible
7. BEGIN → INSERT → SAVEPOINT → INSERT → ROLLBACK TO → COMMIT → correct subset
8. Autocommit: each INSERT is independent transaction

### Scale + Splits
9. INSERT 10K rows → verify tree structure (internal nodes exist)
10. INSERT 10K → SELECT range 500-600 → verify 100 rows in PK order
11. INSERT 10K → UPDATE all → verify all updated
12. INSERT 10K → DELETE half → VACUUM → INSERT new → verify total count

### Wire Protocol (if applicable)
13. pymysql: CREATE clustered → INSERT → SELECT → verify via MySQL protocol

## Benchmarks

### local_bench.py with clustered tables

Modify `benches/comparison/local_bench.py` to create clustered tables (with PRIMARY KEY)
and run the standard benchmark suite. Compare results:

| Scenario | Heap (before) | Clustered (after) | Expected improvement |
|---|---|---|---|
| select_pk | 🔴 0.74x | ~1.0x | Eliminates heap fetch |
| select_range | 🟡 0.77x | ~1.0x | Single pass leaf chain |
| update_range | 🔴 0.44x | ~0.75x+ | No double read |
| insert | 🟡 0.99x | ~0.95x | Similar (B-tree insert) |
| select (full) | 🟡 0.94x | ~1.0x | Leaf chain vs heap chain |
| count | 🟢 1.82x | ~1.5x | Full scan (no fast path) |
| delete | 🟢 2.11x | ~2.0x | Similar |

### New benchmark: clustered_bench.py

Dedicated benchmark for clustered-specific operations:
```
Scenarios:
  pk_point_lookup   — 100 random PK lookups
  pk_range_scan     — 5000-row PK range
  full_scan         — all 50K rows
  in_place_update   — UPDATE non-key column on 5000 rows
  delete_and_vacuum — DELETE 25K + VACUUM
```

## Acceptance criteria

- [ ] All 12+ integration tests pass
- [ ] Benchmark results show improvement for PK operations
- [ ] No regression for non-PK operations
- [ ] Wire protocol smoke test passes (if applicable)
- [ ] Results documented in docs/fase-39.md
- [ ] docs/progreso.md updated with all Phase 39 subfases marked complete

## Dependencies

- ALL of 39.1-39.19 must be complete
