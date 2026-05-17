# Closing the SQLite Performance Gap

> **Status**: in progress. Attack 3.A landed 2026-05-16. ~2× across-the-board win.

AxiomDB is being prepared for an embedded-library release (similar to SQLite
or DuckDB). SQLite is the de-facto standard for embedded SQL databases — it
is also the reference and the target. This document tracks our work to
close the performance gap on the embedded path.

## How we measure

All numbers come from `axiomdb_bench --compare`, a Rust-native harness that
runs **both** engines in the same process (no Python, no TCP, no Docker).
SQLite is loaded via `rusqlite` with bundled SQLite 3.51 and configured for
identical durability:

- SQLite: `PRAGMA journal_mode=WAL`, `synchronous=NORMAL`
- AxiomDB: defaults (`fsync=true`, WAL-native)

Same SQL text, same schema, same row count, same fresh DB per iteration.

```bash
cargo run -p axiomdb-bench-comparison --release -- --compare --rows 10000
```

## Why the gap exists (initial diagnostic, 2026-05-16)

Per-row engine work in `enqueue_clustered_insert_ctx` is **~1 µs/row** —
competitive with SQLite. The 62× gap on INSERT was almost entirely
**per-statement overhead**: every individual SQL statement was paying for
catalog lookups, dispatcher dispatch, clones, and trigger wrapping. SQLite
avoids this with prepared statements; AxiomDB was re-resolving the table
catalog on every statement inside a transaction.

| Layer                        | µs/row (baseline) |
|------------------------------|------------------:|
| parse                        |              1.93 |
| txn.snapshot                 |              0.02 |
| analyze_cached               |              0.37 |
| **execute_with_ctx**         |        **55-110** |
| (per-row loop, ≤ 1 µs)       |                 1 |
| (per-statement overhead, 50-100 µs) |       49-99 |

## Attack 3.A — Versioned ResolvedTable cache

**Date**: 2026-05-16
**Spec**: [`spec-insert-setup-dedup-A.md`](../specs/fase-perf-sqlite-gap/spec-insert-setup-dedup-A.md)
**Plan**: [`plan-insert-setup-dedup-A.md`](../specs/fase-perf-sqlite-gap/plan-insert-setup-dedup-A.md)
**Inspiration**: SQLite's schema-cookie pattern (`research/sqlite/src/prepare.c:518-526`)

### The change

Before: `resolve_table_cached` **bypassed** its own `ResolvedTable` cache
whenever an explicit transaction was active. Reason: DDL or `TRUNCATE`
inside a transaction can change the catalog mid-flight, so a cached entry
might be stale.

After: cached entries are now served inside transactions too, validated
against the table's `schema_version` (a u64 already present in every
`TableDef`, bumped by DDL via `CatalogWriter::bump_table_schema_version`).
On every lookup we do **one** catalog row read to confirm the cached
`schema_version` is still current; if it matches, return the cached entry.

Plus a latent invariant fix: `update_table_root` and `replace_index_def`
now bump `schema_version` too. Without this, bulk DELETE root rotation and
VACUUM page-free would leave the cache holding stale `root_page_id`s
(caught by existing tests
`integration_delete_apply::bulk_delete_savepoint_rollback` and
`integration_clustered_vacuum::vacuum_keeps_secondary_queries_working`).

### Results

| Scenario       | Before  | After   | Δ        | vs SQLite |
|----------------|--------:|--------:|---------:|----------:|
| insert_batch   | 8.8K    | 21.3K   | **+142%**| 47× (was 62×) |
| crud/select    | 2.35M   | 4.14M   | +76%     | 4.9×          |
| full_scan      | 2.61M   | 5.11M   | +96%     | 4.2×          |
| select_where   | 1.84M   | 3.77M   | +105%    | 8.9×          |
| point_lookup   | 4.3K    | 8.7K    | +102%    | 23×           |
| range_scan     | 325K    | 709K    | +118%    | 29×           |
| count_star     | 1.6K    | 4.3K    | +169%    | 71×           |

Roughly 2× across the board. The win is universal because
`resolve_table_cached` is on the hot path of every DML/SELECT statement,
not just INSERT.

### Cross-path impact

This change is in the SQL engine — every path through `Db::run(sql)`
benefits proportionally:

| Path                                   | Expected speedup |
|----------------------------------------|------------------|
| Embedded Rust (`Db::execute`)          | 2× confirmed     |
| C FFI / Python `bindings/python/axiomdb.py` | ~2× expected (FFI overhead is ~0) |
| MySQL wire — batched INSERT in one txn | ~2× expected     |
| MySQL wire — autocommit INSERT         | ~1.5-2× expected (TCP dominates) |
| MySQL wire — point SELECT              | ~1.3-1.7× expected (TCP + serialization share what's left) |

The wire-protocol numbers will be re-measured when Attack 3.B lands —
the current focus is closing more of the engine gap before validating
end-to-end wire numbers.

### What didn't land — Attack 3.A.2

A second optimization was attempted (per-`ConnectionTxn` `catalog_dirty`
flag, skip the catalog probe when no DDL has been issued). It was
reverted because a VACUUM run in autocommit on its own
`ConnectionTxn` flips the catalog state, but the user's next
transaction starts with `catalog_dirty = false` and the fast path
would serve a stale entry.

The correct A.2 design needs **session-level** dirty tracking with
clear-on-revalidate semantics. Tracked as a follow-up.

## What's next

Roughly in order of expected impact:

1. **Attack 3.B — clone removal**: strip the per-statement `.clone()`s
   and `.to_vec()`s in `enqueue_clustered_insert_ctx`
   (`resolved.def.clone()`, `schema_cols.to_vec()`, `primary_idx.clone()`).
   Estimated 1.3-1.5× additional.
2. **Attack 3.A.2 — session-level dirty tracking**: skip the
   per-call catalog probe when safe. Estimated 5-15K extra rows/s.
3. **Attack 2 — statement fingerprinting**: cache by query shape (strip
   literals), reuse plan + ResolvedTable + col_positions per shape.
   SQLite-style prepared statement reuse. Estimated 5-10× on workloads
   with literals.
4. **Attack 4 — per-row engine work**: only attempted after 1-3 land;
   the 1µs/row is already competitive but `prepare_row (codec+PK)` at
   0.6 µs is the dominant cost there.

Goal: close to within 5× of SQLite on all scenarios before the embedded
`v0.5.0-alpha` release.
