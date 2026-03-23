# Design Decisions

This page documents the most consequential architectural choices made during NexusDB's
design. Each entry explains the alternatives considered, the reasoning, and the
trade-offs accepted.

---

## Storage

### mmap over a Custom Buffer Pool

| Aspect | Decision |
|--------|----------|
| **Chosen** | `memmap2::MmapMut` — OS-managed page cache |
| **Alternatives** | Custom buffer pool (like InnoDB), `io_uring` direct I/O |
| **Phase** | Phase 1 (Storage Engine) |

**Why mmap:**
- The OS page cache provides LRU eviction, readahead prefetching, and dirty page
  write-back for free. Implementing these correctly in user space takes months of
  engineering work.
- Pages returned by `read_page()` are `&Page` references directly into the mapped
  memory — zero copy from kernel to application.
- MySQL InnoDB maintains a separate buffer pool on top of the OS page cache. The same
  physical page lives in RAM twice (once in the kernel page cache, once in the buffer
  pool). mmap eliminates the second copy.
- `msync(MS_SYNC)` provides the same durability guarantee as `fsync` for WAL and
  checkpoint flushes.

**Trade-offs accepted:**
- No fine-grained control over eviction policy (OS uses LRU; a custom pool could use
  clock-sweep with hot/cold zones).
- On 32-bit systems, mmap is limited by the address space. Not a concern for a modern
  64-bit server database.
- mmap I/O errors manifest as `SIGBUS` rather than `Err(...)`. These are handled with
  a signal handler that converts `SIGBUS` to `DbError::Io`.

---

### 16 KB Page Size

| Aspect | Decision |
|--------|----------|
| **Chosen** | 16,384 bytes (16 KB) |
| **Alternatives** | 4 KB (SQLite), 8 KB (PostgreSQL), 8 KB (original db.md spec) |
| **Phase** | Phase 1 |

**Why 16 KB:**
- The B+ Tree ORDER constants (ORDER_INTERNAL = 223, ORDER_LEAF = 217) yield a highly
  efficient fan-out with 16 KB pages. At 4 KB, the order would be ~54 for internal
  nodes — requiring 4× more page reads for the same number of keys.
- At 16 KB, a tree covering 1 billion rows has depth 4. At 4 KB, depth 5 (25% more
  I/O for every lookup).
- OS readahead typically prefetches 128–512 KB, making 16 KB the sweet spot: small
  enough that random access is not wasteful, large enough for sequential workloads.
- 64-byte header leaves 16,320 bytes for the body — a natural fit for the
  `bytemuck::Pod` structs that avoid alignment issues.

---

## Indexing

### Copy-on-Write B+ Tree

| Aspect | Decision |
|--------|----------|
| **Chosen** | CoW B+ Tree with `AtomicU64` root swap |
| **Alternatives** | Traditional B+ Tree with read-write locks; LSM-tree (like RocksDB); Fractal tree |
| **Phase** | Phase 2 (B+ Tree) |

**Why CoW B+ Tree:**
- Readers are completely lock-free. A `SELECT` on a billion-row table never blocks
  any concurrent `INSERT`, `UPDATE`, or `DELETE`.
- MVCC is "built in" — readers hold a pointer to the old root and see a consistent
  snapshot of the tree, exactly as MVCC requires.
- No deadlocks are possible during tree traversal (locks are never held during reads).
- Writes amplify by O(log n) page copies, but at depth 4 this is 4 × 16 KB = 64 KB
  per insert — acceptable for the target workload (OLTP, not write-heavy OLAP).

**Why not LSM:**
- LSM-trees have superior write throughput (sequential I/O only) but inferior read
  performance (must check multiple levels). NexusDB's target is OLTP with read-heavy
  workloads. A B+ Tree point lookup is O(log n) I/Os; an LSM lookup is O(L) compaction
  levels, each potentially requiring a disk seek.
- Compaction in LSM introduces unpredictable write amplification spikes that are
  difficult to tune for latency-sensitive OLTP.

### next_leaf Not Used in Range Scans

| Aspect | Decision |
|--------|----------|
| **Chosen** | Re-traverse from root to find the next leaf on each boundary crossing |
| **Alternatives** | Keep the `next_leaf` linked list consistent under CoW |
| **Phase** | Phase 2 |

**Why:** Under CoW, `next_leaf` pointers in old leaf pages point to other old pages
that may have been freed. Maintaining a consistent linked list under CoW requires
copying the previous leaf on every insert near a boundary — but the previous leaf's
page_id is not known during a top-down write path without additional bookkeeping.

The cost of the adopted solution (O(log n) per leaf boundary) is acceptable: for a
10,000-row range scan across ~47 leaves (217 rows/leaf), there are 46 boundary
crossings, each costing 4 page reads = 184 extra page reads. At a measured scan time
of 0.61 ms for 10,000 rows, this is within the 45 ms budget by a factor of 73.

---

## Durability

### WAL Without Double-Write Buffer

| Aspect | Decision |
|--------|----------|
| **Chosen** | WAL with per-page CRC32c; no double-write buffer |
| **Alternatives** | Double-write buffer (MySQL InnoDB); full page WAL images (PostgreSQL) |
| **Phase** | Phase 3 (WAL) |

**Why no double-write:**
- MySQL writes each page twice: once to the doublewrite buffer and once to the actual
  position. The doublewrite buffer protects against torn writes (partial page writes
  due to power failure mid-write).
- NexusDB protects against torn writes with a CRC32c checksum per page. If a page has
  an invalid checksum on startup, it is reconstructed from the WAL. This requires the
  WAL to contain the information needed for reconstruction — which it does (the WAL
  records the full new_value for each UPDATE/INSERT).
- Eliminating the double-write buffer halves the disk writes for every dirty page
  flush.

**Trade-off:** Recovery requires reading more WAL data. If many pages are corrupted
(e.g., a full power failure after a long write batch), recovery replays more WAL
entries. In practice, with modern UPS and filesystem journaling, full-file corruption
is rare. The WAL's CRC32c catches partial writes reliably.

### Physical WAL (not Logical WAL)

| Aspect | Decision |
|--------|----------|
| **Chosen** | Physical WAL: records (page_id, slot_id, old_bytes, new_bytes) |
| **Alternatives** | Logical WAL: records SQL-level operations (INSERT INTO t VALUES...) |
| **Phase** | Phase 3 |

**Why physical:**
- Recovery is redo-only: replay each committed WAL entry at its exact physical
  location. No UNDO pass required (uncommitted changes are simply ignored).
- Physical location (page_id, slot_id) allows direct seek to the affected page —
  O(1) per WAL entry, not O(log n) B+ Tree traversal.
- The WAL key encodes `page_id:8 + slot_id:2` in 10 bytes, making the physical
  location self-contained in the WAL record.

**Trade-off:** Physical WAL entries are larger than logical ones (they contain the
full encoded row bytes, not a SQL expression). For a row with 100 bytes of data,
the WAL entry is ~100 + 43 bytes overhead = ~143 bytes. A logical WAL entry might
be smaller for simple inserts. However, the simplicity and speed of redo-only
physical recovery outweighs the size difference.

---

## SQL Processing

### logos for Lexing

| Aspect | Decision |
|--------|----------|
| **Chosen** | `logos` crate — compiled DFA |
| **Alternatives** | `nom` combinators; `pest` PEG; hand-written lexer; `lalrpop` |
| **Phase** | Phase 4.2 (SQL Lexer) |

**Why logos:**
- logos compiles all token patterns (keywords, identifiers, literals) into a single
  DFA at build time. Runtime cost per character is a table lookup — 1–3 CPU instructions.
- The `ignore(ascii_case)` attribute makes keyword matching case-insensitive with no
  runtime cost (the DFA is built with both cases folded).
- Zero-copy: `Ident(&'src str)` slices into the input without heap allocation.
- **Measured throughput: 9–17× faster than sqlparser-rs** for the same inputs.

nom is an excellent choice for context-free parsing with backtracking but is
over-engineered for a lexer: a lexer is a regular language (no backtracking needed),
and DFA is the optimal algorithm for it.

### Zero-Copy Tokens

| Aspect | Decision |
|--------|----------|
| **Chosen** | `Token::Ident(&'src str)` — lifetime-tied reference into the input |
| **Alternatives** | `Token::Ident(String)` — owned heap allocation; `Token::Ident(Arc<str>)` |
| **Phase** | Phase 4.2 |

**Why zero-copy:**
- Heap allocation per identifier would cost ~30 ns on modern hardware (involving a
  `malloc` call). For a query with 20 identifiers, that is 600 ns of allocation overhead.
- At 2M queries/s (the target throughput), 600 ns per query consumes 1.2 s per second
  of CPU time in allocations — impossible to sustain.
- Zero-copy tokens require the input string to outlive the token stream, which is a
  natural constraint: the input is always available until the query finishes.

---

## MVCC Implementation

### RowHeader in Heap Pages (not Undo Tablespace)

| Aspect | Decision |
|--------|----------|
| **Chosen** | MVCC metadata (`xmin`, `xmax`, `deleted`) in each heap row |
| **Alternatives** | Separate undo tablespace (MySQL InnoDB); version chain in B+ Tree (PostgreSQL MVCC heap) |
| **Phase** | Phase 3 (TxnManager) |

**Why inline RowHeader:**
- A historical row version is visible in its original heap location. No additional
  I/O is needed to read old versions — they are in the same page as the current version.
- MySQL's undo tablespace (`ibdata1`) requires additional I/O for reads that need old
  row versions (the reader follows a pointer chain from the clustered index into the
  undo tablespace).
- Inline metadata is simpler to implement and audit.

**Trade-offs:**
- Dead rows occupy space in the heap until `VACUUM` (Phase 9) cleans them up.
- The `RowHeader` adds 24 bytes overhead per row. For a table with 50-byte average
  rows, this is 32% overhead. Acceptable for the generality it provides.

---

## Collation

### UCA Root as Default Collation

| Aspect | Decision |
|--------|----------|
| **Chosen** | Unicode Collation Algorithm (UCA) root for string comparison |
| **Alternatives** | ASCII byte order; locale-specific collation; C locale (PostgreSQL default) |
| **Phase** | Phase 4 (Types) |

**Why UCA root:**
- ASCII byte order (`strcmp`) gives incorrect ordering for most non-English text:
  'ä' sorts after 'z' in ASCII, but should sort near 'a'.
- UCA root is locale-neutral (deterministic across any server environment) while still
  correct for most languages.
- MySQL's default collation (utf8mb4_general_ci) is not standards-compliant.
- UCA root is implemented by the `icu` crate — same algorithm used by modern browsers
  for `Intl.Collator`.

---

## Content-Addressed BLOB Storage (Planned Phase 6)

| Aspect | Decision |
|--------|----------|
| **Planned** | SHA-256 content address as the BLOB key in a dedicated BLOB store |
| **Alternatives** | Inline BLOB in the heap (PostgreSQL TOAST); external file reference |
| **Phase** | Phase 6 |

**Why content-addressed:**
- Two rows storing the same attachment (e.g., a company logo in every invoice) share
  exactly one copy on disk. Deduplication is automatic and requires no extra schema.
- The BLOB store is append-only with immutable entries — no locking on BLOB reads.
- Deletion is handled by reference counting: when the last row referencing a BLOB
  is deleted, the BLOB can be garbage collected.
