# Plan: Gap Closure — Implementation

## Optimization 1: Range scan prefetch batch

### Files to modify
- `crates/axiomdb-index/src/iter.rs` — RangeIter: prefetch on leaf boundary
- `crates/axiomdb-index/src/tree.rs` — range_in(): pass prefetch depth

### Algorithm
```
When RangeIter exhausts current leaf:
  1. Read next_leaf from current page (already cached)
  2. Call storage.prefetch_hint(next_leaf_pid, 4) → hint next 4 pages
  3. Move to next_leaf (now likely in OS page cache)
```

### Implementation phases
1. Cache `next_leaf_pid` in RangeIter state (avoid double page read)
2. On leaf boundary: call `prefetch_hint(next+1, 3)` for next 3 siblings
3. Test: verify same results, measure latency improvement

### Tests
- Existing range scan tests must pass unchanged
- New: benchmark 10K-row range scan before/after

### Anti-patterns
- Do NOT prefetch on every `next()` call — only at leaf boundary
- Do NOT block on prefetch — it's a hint, not a requirement

---

## Optimization 2: Wire write batching

### Files to modify
- `crates/axiomdb-network/src/mysql/handler.rs` — accumulate packets before flush
- `crates/axiomdb-network/src/mysql/result.rs` — serialize_query_result returns bytes

### Algorithm
```
Current:  for each packet → write_all(socket)
Proposed: for each packet → buf.extend(frame(packet))
          write_all(socket, &buf)  ← single syscall
```

### Implementation phases
1. In handler.rs: collect all framed packets into a single Vec<u8>
2. Write the entire buffer in one `write_all()` call
3. For large results (>1MB): chunk into 64KB flushes

### Tests
- Wire protocol tests (pymysql integration) must pass
- Benchmark select_pk before/after

---

## Optimization 3: Aggregate key reuse

### Files to modify
- `crates/axiomdb-sql/src/executor/aggregate.rs` — execute_select_grouped_hash

### Algorithm
```
Current:  for each row → key_bytes = group_key_bytes_session(&values) // NEW Vec
Proposed: let mut key_buf = Vec::with_capacity(64);
          for each row → key_buf.clear(); append_key(&key_values, &mut key_buf)
                       → groups.entry(key_buf.clone())  // clone only for HashMap
```

### Implementation phases
1. Create `append_group_key_session(values, buf)` that appends to existing buffer
2. Replace `group_key_bytes_session()` call with append pattern
3. Only clone key_buf when inserting new group (not on every row)

### Tests
- All GROUP BY tests must pass unchanged
- Benchmark aggregate before/after

---

## Optimization 4: Adaptive Hash Index

### Files to create/modify
- `crates/axiomdb-index/src/ahi.rs` — NEW: AdaptiveHashIndex struct
- `crates/axiomdb-index/src/tree.rs` — lookup_in(): check AHI first
- `crates/axiomdb-index/src/lib.rs` — export AHI module

### Data structure
```rust
pub struct AdaptiveHashIndex {
    entries: HashMap<u64, RecordId>,  // CRC-32C(key) → RecordId
    access_counts: HashMap<u64, u16>, // leaf_page_id → consecutive accesses
}

const AHI_BUILD_THRESHOLD: u16 = 64;  // MariaDB uses 100
```

### Algorithm
```
lookup_in(root, key):
  1. hash = crc32c(key)
  2. if ahi.contains(hash) → return ahi[hash] (O(1))
  3. else → B-Tree traversal → result
  4. ahi.record_access(leaf_page_id)
  5. if access_count > threshold → ahi.build_for_page(leaf_page_id)
```

### Tests
- Existing B-Tree tests must pass (AHI is transparent fast-path)
- New: test AHI build after N accesses, test AHI miss fallback
- Benchmark select_pk before/after

---

## Optimization 5: Vectorized aggregate chunks

### Files to modify
- `crates/axiomdb-sql/src/executor/aggregate.rs` — chunk-based processing

### Algorithm
```
Split combined_rows into chunks of 1024:
  for chunk in rows.chunks(1024):
    1. Extract GROUP BY values for all 1024 rows → Vec<Vec<Value>>
    2. Compute hashes for all 1024 keys → Vec<u64>
    3. For each row in chunk: HashMap lookup + accumulator update
       (amortized: cache-friendly sequential access)
```

### Tests
- All GROUP BY + aggregate tests must pass
- Benchmark aggregate before/after (expect ≥25% improvement)

---

## Optimization 6: Read-ahead 8-16 pages

### Files to modify
- `crates/axiomdb-index/src/iter.rs` — adaptive prefetch depth

### Algorithm
```
Initial: prefetch_depth = 4
After 4 consecutive boundary crossings: prefetch_depth = 8
After 8 consecutive: prefetch_depth = 16
Cap at 16 (not 64 like MariaDB — our pages are 16KB vs 16KB)
```

### Tests
- Range scan correctness unchanged
- Benchmark large range scans (10K+ rows)

---

## Execution order

1. **Opt 1** (range prefetch) — most testable, independent
2. **Opt 3** (agg key reuse) — quick win, independent
3. **Opt 2** (wire batching) — independent, measurable
4. **Opt 4** (AHI) — larger effort, high impact
5. **Opt 5** (vectorized agg) — depends on 3
6. **Opt 6** (read-ahead 16) — depends on 1

## Risks
- AHI invalidation on B-Tree splits: lazy rebuild (entry misses after split, rebuilt on next access)
- Wire batching memory: cap at 1MB per query result buffer
- Aggregate key reuse: HashMap requires owned keys — clone only on new group insert
