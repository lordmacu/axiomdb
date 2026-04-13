# Project State

## Current (2026-04-13)

**Active phase:** Phase 11 — Advanced Types
**Active subphase:** 11.25c JSON construction helpers (`jsonb_build_object`, `jsonb_build_array`, `to_json`) — ✅ closed (row_to_json deferred). Remaining fase 11 (MySQL+PG path): 11.18c, 11.21h, 11.25d.

### Phase 11 subphase status

| Subphase | Status |
|---|---|
| 11.2d BLOB refcount | ✅ closed |
| 11.4 Native JSON | ✅ closed |
| 11.7 Advanced FTS (boolean, phrase, prefix) | ✅ closed |
| 11.8 Buffer Pool Manager | ✅ closed |
| 11.9 + 11.10 Prefetch + Write Combining | ✅ closed |
| 11.16 Binary JSONB + JSONPath | ✅ closed |
| 11.17 GIN index for JSONB | ✅ closed |
| 11.18a / 11.18b (partial) JSONB operators | ✅ closed — 11.18c deferred (needs `TEXT[]`) |
| 11.19a / 11.19b / 11.19c SQL/JSON query functions | ✅ closed |
| 11.20a `JSON_TABLE` flat | ✅ closed |
| 11.20b `JSON_TABLE` single-level NESTED | ✅ closed |
| 11.20c `JSON_TABLE` multi-sibling + multi-level NESTED | ✅ closed |
| 11.20d1 `JSON_TABLE` WRAPPER / QUOTES / PASSING | ✅ closed |
| 11.20d2 `JSON_TABLE` first FROM + CROSS/OUTER APPLY | ✅ closed |
| 11.20d3 `JSON_TABLE` LATERAL-correlated doc + PASSING | ✅ closed |
| **11.20d4** `JSON_TABLE` as UPDATE/DELETE source | ✅ closed — **Phase 11.20 COMPLETE** |
| 11.21a–g JSONPath parity | ✅ closed (through path arithmetic in filters) — 11.21h pending planner pushdown |
| 11.22a / 11.22b JSONB mutations | ✅ closed |
| 11.23a / 11.23b / 11.23d / 11.23e / 11.23f JSON Schema | ✅ closed — 11.23c catalog persistence pending |
| 11.24a / 11.24b / 11.24d (partial) Oracle JSON | ✅ closed — 11.24c dot-notation pending |
| **11.25** JSON SRF + aggregates + construction helpers (PG streaming parity) | ⬜ scoped — 4 subfases 11.25a-d |

### 11.20 follow-ups

- **11.20b** — Single-level `NESTED PATH` with LEFT-OUTER NULL padding + per-level ordinality.
- **11.20c** — Multi-sibling (`UNION` semantics) and multi-level NESTED PATH.
- **11.20d1** — WRAPPER/QUOTES, `PASSING` on the row path. ✅ closed.
- **11.20d2** — JSON_TABLE as first FROM + JOINs; CROSS/OUTER APPLY parser sugar. ✅ closed.
- **11.20d3** — LATERAL-correlated `doc` / PASSING referencing outer columns + `LATERAL` keyword. ✅ closed.
- **11.20d4** — JSON_TABLE as UPDATE/DELETE source (MERGE deferred until MERGE lands).

### Completed phases

Phases 1–10, 22b, 39, 40 — all closed. See `docs/progreso.md` for subphase details.

### Key open gaps (deferred)

- Phase 4: 4.10f, 4.11b, 4.4g, 4.22f, 4.22e
- Phase 5: 5.2d, 5.5b
- Phase 6: 6.5b, 6.6d, 6.6c
- Phase 39: MVCC version chains, root checkpoint persistence, FK clustered children, separator overflow, 39.19b uncommitted

### GitHub

Account: `lordmacu` (`lordmacu@users.noreply.github.com`). Push target: `origin main`.
