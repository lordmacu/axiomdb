# Spec: 11.8 — Buffer Pool Manager

## What to build
User-space LRU page cache layered on top of mmap. Hot pages stay in RAM without
re-reading through the OS page cache. Transparent to executor — `read_page()`
automatically routes through the buffer pool.

## Research
- **InnoDB** (buf0buf.cc): hash table + LRU with young/old split (25% boundary).
  Scan-resistant: new pages start "old", promoted to "young" on second access.
- **PostgreSQL** (bufmgr.c): clock-sweep replacement, pin/unpin, partitioned hash.

## Design: 16-shard LRU over mmap
- 16 `Mutex<CacheShard>` partitioned by `page_id % 16`
- Each shard: `HashMap<u64, CacheEntry>` + `VecDeque<u64>` LRU order
- `CacheEntry`: `Arc<PageRef>` (cheap clones), access_time, pin_count
- Default capacity: 1024 pages (16 MB), configurable
- Eviction: pop LRU tail, skip if pin_count > 0

## Acceptance criteria
- [ ] Buffer pool caches read_page() results
- [ ] Second read of same page returns cached copy (no mmap read)
- [ ] LRU eviction when pool full
- [ ] Pin/unpin prevents eviction of in-use pages
- [ ] select_pk benchmark improves (fewer re-reads)
- [ ] No regression on write-heavy benchmarks
- [ ] Configurable capacity via DbConfig
