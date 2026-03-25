# Spec: Fill Factor (Phase 6.8)

## What to build (not how)

Fill factor controls how full a B-Tree leaf page is allowed to get before it
splits. A fill factor of 70 means each leaf page is split when it reaches 70%
capacity, leaving 30% free space for future inserts without triggering another
split.

Primary benefit: **reduce split frequency for append-heavy or range-insert
workloads** — e.g., time-series tables, log tables, auto-increment primary keys.
Lower fill factor → more splits at CREATE INDEX time, but fewer during subsequent
INSERT operations → more predictable, lower-variance write latency.

Reference: PostgreSQL `BTREE_DEFAULT_FILLFACTOR = 90` for leaf pages,
`BTREE_NONLEAF_FILLFACTOR = 70` (fixed) for internal pages.
AxiomDB follows the same split: only leaf pages respect the configured fill factor.

---

## Syntax

```sql
CREATE INDEX idx_ts ON events(created_at) WITH (fillfactor = 70);
CREATE UNIQUE INDEX uq_email ON users(email) WITH (fillfactor = 80);

-- Default (fillfactor = 90) — equivalent to omitting WITH clause:
CREATE INDEX idx_x ON t(x);
```

Valid range: **10 – 100** (inclusive). Values outside this range return a
`ParseError`. The default when `WITH` is omitted is **90**.

---

## Inputs / Outputs

### DDL — CREATE INDEX WITH (fillfactor = N)

**Input:** `CREATE [UNIQUE] INDEX name ON table(col) [WHERE pred] [WITH (fillfactor=N)]`

**Output:** `QueryResult::Empty` on success.

**Errors:**
- `ParseError { message: "fillfactor must be between 10 and 100" }` — value out of range
- `ParseError { message: "unknown index option: X" }` — unrecognised `WITH` key
- All existing `CREATE INDEX` errors (TableNotFound, etc.)

### Effect on B-Tree inserts

When `fillfactor = F` and `ORDER_LEAF = 217`:
- **Split threshold** = `max(1, ceil(F × 217 / 100))`
  - `F=100` → 217 (same as today; no behavior change)
  - `F=90`  → 196 (pages split at 90% full; default)
  - `F=70`  → 152 (pages split at 70% full)
  - `F=50`  → 109 (pages split at 50% full)
  - `F=10`  → 22  (minimum)

A leaf page is written in-place when `num_keys < threshold`. It splits when
`num_keys >= threshold`, dividing keys evenly (50-50) between two new pages.
After a split both new pages hold approximately `threshold / 2` keys.

Internal pages always split at `ORDER_INTERNAL` (= current behavior, not
configurable). This matches PostgreSQL's design: `BTREE_NONLEAF_FILLFACTOR = 70`
is fixed and independent of the index fill factor setting.

### Backward-compatible catalog storage

`fillfactor` is appended to the `IndexDef` on-disk row after the predicate
section. Pre-6.8 rows lack this byte; `from_bytes` returns `fillfactor = 90`
(default) for them.

```text
[...existing fields...][pred_len:2][pred_sql bytes]
[fillfactor: 1 byte]   ← NEW; absent on pre-6.8 rows → defaults to 90
```

---

## Use cases

### 1. Default fill factor (90) — no change from today
```sql
CREATE INDEX idx_x ON t(x);
-- leaf pages split at 196/217 keys (90%) — same as current behavior
```

### 2. Low fill factor for sequential inserts
```sql
-- Time-series table: events inserted in chronological order.
CREATE INDEX idx_ts ON events(created_at) WITH (fillfactor = 70);
-- Leaf pages split at 152/217 keys.
-- After build: each page ~35% full (152/2 after split).
-- New events fill the rightmost page to 70% before another split.
```

### 3. High fill factor for read-heavy, write-rare
```sql
CREATE UNIQUE INDEX uq_email ON users(email) WITH (fillfactor = 100);
-- Leaf pages filled completely — most compact possible.
-- Every insert may trigger a split, but storage is minimized.
```

### 4. Pre-6.8 database
```
-- index row has no fillfactor byte → from_bytes returns fillfactor = 90
-- insert_in receives fillfactor = 90 → threshold = 196 → current behavior
```

---

## Acceptance criteria

- [ ] `CREATE INDEX ... WITH (fillfactor=N)` persists `fillfactor = N` in catalog
- [ ] `CREATE INDEX` without `WITH` persists `fillfactor = 90`
- [ ] `fillfactor` outside 10–100 returns `ParseError`
- [ ] Unknown `WITH` key returns `ParseError { "unknown index option: X" }`
- [ ] `insert_in` uses `fillfactor` from `IndexDef` for split threshold
- [ ] `fillfactor = 100` produces identical behavior to current (no regression)
- [ ] `fillfactor = 70`: leaf pages have ≤ 152 entries after insert sequence
- [ ] Pre-6.8 rows (no fillfactor byte) open without error and use fillfactor = 90
- [ ] `insert_into_indexes` reads `idx.fillfactor` and passes to `BTree::insert_in`
- [ ] `execute_create_index` passes `fillfactor` from the statement to `BTree::insert_in`
- [ ] Integration tests covering all acceptance criteria above

---

## Out of scope

- `WITH (fillfactor=N)` on tables (not indexes) — different semantic, Phase 9+
- Rightmost-page optimization (left page at `FF%`, right nearly empty) — PostgreSQL
  `SPLIT_DEFAULT` for sequential patterns — Phase 6.9 if benchmarks show need
- `REINDEX` to apply a new fill factor to an existing index — Phase 6.15
- `SHOW INDEX` / `information_schema.STATISTICS` exposing fill factor — Phase 6.9

## ⚠️ DEFERRED
- Rightmost split optimization → Phase 6.9

---

## Dependencies

- Phase 6.1–6.3: `IndexDef`, `BTree::insert_in` (already exists)
- Phase 6.7: `IndexDef.predicate: Option<String>` — the fillfactor byte is
  appended AFTER the predicate section; reading order matters
