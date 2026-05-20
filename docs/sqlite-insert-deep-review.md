# SQLite INSERT path — deep review

> Research notes from reading SQLite 3.51 source (in `research/sqlite/src/`)
> to find techniques AxiomDB could replicate. Recorded 2026-05-17 after
> Attack 2 closed partial.

The goal is to understand why SQLite hits **1.0M INSERT rows/s** on the
shared embedded bench while AxiomDB sits at **21K** — and to identify
what's structurally fixable.

---

## Map: the SQLite INSERT pipeline

A single `db.run("INSERT INTO t VALUES (1, 'a')")` from a host language
that doesn't keep a prepared statement open (≈ what AxiomDB does
internally on every call) walks through these layers:

```
SQL text
   │
   ▼   sqlite3_prepare_v2 (src/prepare.c)
parse + ast + analyze + cookie check
   │
   ▼   sqlite3Insert (src/insert.c:894)
Vdbe bytecode generation (OP_OpenWrite, OP_NewRowid,
                          OP_MakeRecord, OP_Insert, ...)
   │
   ▼   sqlite3_step (src/vdbeapi.c:913)
loop: sqlite3VdbeExec → big switch dispatch
   │
   ▼   case OP_Insert (src/vdbe.c:5748)
sqlite3BtreeInsert(cursor, BtreePayload, flags, seekResult)
   │
   ▼   sqlite3BtreeInsert (src/btree.c:9394)
position cursor → fillInCell → insertCellFast OR overwrite
   │
   ▼   sqlite3PagerWrite (src/pager.c:6234)
mark page dirty, copy to journal/WAL if first touch
   │
   ▼   sqlite3WalFrames (src/wal.c:4029, on COMMIT only)
flush ALL dirty pages in one syscall sequence + ONE fsync
```

Per-statement cost on the **legacy** path (no prepared statement reuse,
which is AxiomDB's situation): everything from `sqlite3_prepare_v2` to
`OP_Halt`. Per-statement cost on the **fast** path (prepared statement
reused via Python `sqlite3` module's `cached_statements`, `?`
placeholders, or repeated calls in a loop): just `sqlite3_step` +
btree work.

The 50× bench gap exists because the bench uses literal-interpolated
SQL where every text is unique → SQLite's host bindings can't text-cache
either. The CRUCIAL win is what happens at the **engine** level even
without statement reuse.

---

## The optimizations that matter (engine-level, not API-level)

### 1. ⭐ **Cursor stays positioned across consecutive INSERTs** (the big one)

`BtCursor` carries cached metadata that survives `sqlite3BtreeInsert`:
- `pCur->pPage` — current leaf, still pinned in the pager cache
- `pCur->info.nKey`, `pCur->info.nSize`, `pCur->info.nPayload` — last
  examined cell
- `pCur->curFlags & BTCF_ValidNKey` — flag that says "the info above is
  fresh"
- `pCur->ix` — index of the cell within the page

The fast path in
[`src/btree.c:9482-9491`](research/sqlite/src/btree.c):

```c
if( (pCur->curFlags&BTCF_ValidNKey)!=0 && pX->nKey==pCur->info.nKey ){
  /* The cursor is pointing to the entry that is to be overwritten */
  if( pCur->info.nSize!=0
   && pCur->info.nPayload==(u32)pX->nData+pX->nZero ){
    /* New entry is the same size as the old. Do an overwrite */
    return btreeOverwriteCell(pCur, pX);
  }
}
```

For multi-row INSERT batches where the cursor naturally walks forward
(monotonic PK), every insert lands either on the same page or the next
one — **no descent from the root, no `btreeMoveto`**. The cursor "drips
along" the rightmost edge of the tree.

This is the optimization invoked by the comment at
[`src/btree.c:9656-9663`](research/sqlite/src/btree.c):

```c
/* There is a subtle but important optimization here too. When inserting
** multiple records into an intkey b-tree using a single cursor (as can
** happen while processing an "INSERT INTO ... SELECT" statement), it
** is advantageous to leave the cursor pointing to the last entry in
** the b-tree if possible. If the cursor is left pointing to the last
** entry in the table, and the next row inserted has an integer key
** larger than the largest existing key, it is possible to insert the
** row without seeking the cursor. This can be a big performance boost. */
```

**Where AxiomDB stands:** every `db.run("INSERT INTO t ...")` opens a
fresh BTreeCursor via the executor, descends from root, releases at end
of statement. We have `HeapAppendHint` (Phase 5.18) on heap tables — a
cached `tail_page_id` — but no equivalent for clustered tables (which is
what most PRIMARY KEY tables go through). And even the heap hint
doesn't keep the cursor *open* across statements.

**Replication candidate:** keep a `LastUsedCursor { table_id, root_page,
btree_cursor }` slot on `SessionContext`. After each
INSERT/UPDATE/DELETE, parking the cursor stays open until the next
statement on the same table can reuse it. Invalidate on:
- Different `table_id` arrives → flush + close
- `schema_version` changes (DDL on this table)
- `clustered_roots[table_id]` differs (root rotated)

Estimated impact: **5-10× on consecutive INSERTs**, the exact scenario
the bench hammers.

---

### 2. ⭐ **`OPFLAG_USESEEKRESULT` — one seek for both PK check and insert**

When a statement has constraint checks, SQLite emits
`OP_NotExists`/`OP_Found` to verify uniqueness; these opcodes leave
the cursor positioned at the would-be insert location and stash that
position in `pCur->seekResult`. The subsequent `OP_Insert` reads the
flag `OPFLAG_USESEEKRESULT` from its p5 and passes the cached position
to `sqlite3BtreeInsert` ([`src/vdbe.c:5808`](research/sqlite/src/vdbe.c)):

```c
seekResult = ((pOp->p5 & OPFLAG_USESEEKRESULT) ? pC->seekResult : 0);
...
rc = sqlite3BtreeInsert(pC->uc.pCursor, &x,
    (pOp->p5 & (OPFLAG_APPEND|OPFLAG_SAVEPOSITION|OPFLAG_PREFORMAT)),
    seekResult);
```

Inside `sqlite3BtreeInsert`, when `seekResult != 0`, the btreeMoveto
call is **skipped entirely** — the cursor is already where it needs
to be.

This is set by `sqlite3CompleteInsertion` at
[`src/insert.c:2837-2840`](research/sqlite/src/insert.c) after
constraint codegen runs.

**Where AxiomDB stands:** per
`crates/axiomdb-sql/src/executor/insert_clustered_ctx.rs:480-512`, our
INSERT path does TWO B-tree probes:
1. The intra-batch PK duplicate check (`pk_dup_check_ns` in
   `--diagnose-insert-deep` = ~0.06 µs/row — actually HashSet, not
   B-tree, so this is fine intra-batch)
2. The actual heap/clustered insert seek + write

For non-batch (autocommit) inserts on clustered tables, the executor
calls `prepare_row_with_ctx` which encodes + sorts + writes, and the
actual leaf-page seek inside the clustered B-tree is paid per row
without any "I already know where you go" hand-off.

**Replication candidate:** add a `LastSeekResult { table_id, leaf_page,
slot_idx }` on the cursor (or on SessionContext alongside the cursor
slot). When `assign_auto_increment` produces the next id, walk the
B-tree once, hand the position to the row writer. For non-auto-inc
PKs, the user-provided key drives a single seek that the writer reuses.

Estimated impact: **30-50% on INSERT** (saves one B-tree descent per
row).

---

### 3. **`OPFLAG_APPEND` — append-mode short-circuit**

`sqlite3Insert` sets `appendFlag = 1`
([`src/insert.c:1516,1540`](research/sqlite/src/insert.c)) when:
- PK is `OP_NewRowid` (auto-generated)
- Table is intkey (rowid) and no explicit PK is given

This flag flows to `OP_Insert.p5 |= OPFLAG_APPEND`, then to
`sqlite3BtreeInsert(... BTREE_APPEND ...)`. Inside, when the cursor
needs to position, `sqlite3BtreeTableMoveto(..., BTREE_APPEND, ...)`
**short-circuits the binary search** — it just goes straight to the
rightmost leaf.

Combined with the cursor-stays-positioned optimization (1), append-only
INSERTs (`INSERT INTO t VALUES (...)` with `t.id AUTO_INCREMENT`)
become nearly O(1) — one page write, no descent.

**Where AxiomDB stands:** AxiomDB's heap path has a "tail hint"
(`HeapAppendHint`) but the clustered path doesn't. AUTO_INCREMENT
inserts into clustered tables walk the B-tree from the top every row.

**Replication candidate:** detect `auto_increment = true` columns at
analyze time; emit an "append intent" flag that the clustered insert
path uses to skip the descent and write to the rightmost leaf directly.

Estimated impact: **2-3× on AUTO_INCREMENT clustered INSERT** (exactly
the bench's `id INT PRIMARY KEY` pattern).

---

### 4. **WAL frames batched + single fsync at COMMIT**

`sqlite3WalFrames` ([`src/wal.c:4029`](research/sqlite/src/wal.c))
writes ALL dirty pages of a transaction in one tight loop, then calls
`sqlite3OsSync` ONCE per commit. Between INSERTs within the txn, pages
sit in the in-memory pager cache, modified but not flushed.

The key invariant: WAL frames are **per-page**, not per-row. 10K
INSERTs into the same handful of pages = a handful of WAL frames.

**Where AxiomDB stands:** AxiomDB also has group commit (Phase 40 —
`FsyncPipeline` and concurrent WAL writer). The structure is similar.
But: AxiomDB's WAL records are per-statement / per-row level (each
INSERT writes its own `record_insert` to the WAL), where SQLite writes
just the dirty *pages*. So our WAL stream has more entries to encode
and fsync, even if the actual disk write is amortized.

**Replication candidate:** evaluate whether AxiomDB could move to
page-level WAL framing (only ship the dirty pages, not the per-row
operations). Bigger architectural change — defer until other wins
land.

Estimated impact: hard to estimate without prototyping; the per-row
WAL encoding cost in AxiomDB is not on the hot path we've been
measuring (most time is in execute scaffolding, not WAL).

---

### 5. **VDBE = pre-compiled flat bytecode, not AST tree-walk**

SQLite's prepared statement is a **flat array of opcodes**
(`Vdbe.aOp[]`). `sqlite3VdbeExec` is a tight switch-on-opcode loop
with a u8 dispatch. No AST traversal at execute time.

The opcodes are designed for cache friendliness:
- Sequential layout — branch prediction friendly
- Registers live in `aMem[]` (preallocated array of `Mem` structs)
- No recursion — everything is a flat goto

For an INSERT with VALUES, the bytecode looks like:
```
OP_OpenWrite      iCursor, table_root, db_idx
OP_Integer        rowid, $val_for_rowid
OP_String         data_buf, $val_for_text_col
... (more value loads)
OP_MakeRecord     reg_array_start, n_columns, record_reg
OP_Insert         iCursor, record_reg, rowid_reg
OP_Halt
```

`sqlite3_step` just runs through these in order. Per-call cost is just
the loop overhead + the underlying btree work.

**Where AxiomDB stands:** AxiomDB's `execute_with_ctx` is a
match-on-`Stmt`-variant dispatcher that recurses through the analyzed
AST. Per-statement cost = dispatcher match + ExecutionContext::new +
conn_txn take/restore + recursive Expr eval per row + trigger wrapper.

The recursion + match isn't free — even though Rust's optimizer is
good, a flat-bytecode interpreter could plausibly be 2-3× faster on
the per-statement scaffolding side.

**Replication candidate:** this is the **largest** project. Compile
the analyzed `Stmt` into a flat `Vec<Instruction>`; replace
`execute_with_ctx` with a tight `for op in plan.ops { match op ... }`
loop. Reuses the existing `expr` types as register-relative ops.

Estimated impact: **2-3× on per-statement overhead**, but
**multi-week** effort. Park for v1.0+; not v0.5.0-alpha.

---

### 6. **VDBE Mem cells = preallocated, in-place reused**

The `aMem[]` array is allocated ONCE at `sqlite3VdbeMakeReady`. Per
row in a `INSERT ... SELECT` loop, the same Mem slots are reused — no
allocator pressure.

Per [`src/vdbe.c:5749-5757`](research/sqlite/src/vdbe.c):
```c
case OP_Insert: {
  Mem *pData;  /* MEM cell holding data for the record */
  Mem *pKey;   /* MEM cell holding key for the record */
  ...
  pData = &aMem[pOp->p2];  /* register access, no alloc */
  pKey = &aMem[pOp->p3];
```

**Where AxiomDB stands:** AxiomDB allocates a `Vec<Value>` per row
inside `materialize_insert_row` and the eval chain. For 10K row INSERT,
that's 10K + heap allocations.

**Replication candidate:** add a `RowScratch` buffer on `SessionContext`
(`Vec<Value>` capacity ≥ max columns seen so far), reuse per row inside
the batch loop. The Vec is cleared (not dropped) between iterations.

Estimated impact: **10-20%** on INSERT — modest but mechanical.

---

### 7. **xferOptimization for `INSERT INTO t1 SELECT * FROM t2`**

`xferOptimization` ([`src/insert.c:3012`](research/sqlite/src/insert.c))
detects the special case of "copy one table to another" and uses
page-level cell transfer (no row decode/encode). It produces the
opcode `OP_RowCell` ([`src/vdbe.c:5847`](research/sqlite/src/vdbe.c))
which calls `sqlite3BtreeTransferRow` — direct cell-to-cell copy.

Result: ~10× faster than the standard codegen path.

**Where AxiomDB stands:** AxiomDB has `ClusteredInsertBatch` staging
which is similar in spirit (batch rows + flush at commit), but it
still decodes/encodes per row. Not a true page-level transfer.

**Replication candidate:** out of scope for the bench (which doesn't
use `INSERT ... SELECT *`), but worth a follow-up for bulk migration
scenarios.

---

## Comparison summary

| Technique | AxiomDB has? | Replication cost | Bench impact |
|-----------|:-------:|------------------|--------------|
| 1. Cursor stays positioned across statements | ❌ (only heap_tail hint, weak) | medium | **5-10×** |
| 2. USESEEKRESULT (single seek for check+insert) | ❌ | low-medium | **30-50%** |
| 3. OPFLAG_APPEND for auto-inc | partial (heap only) | low | **2-3×** on AUTO_INCREMENT |
| 4. Per-page WAL framing | ❌ (per-row) | high | hard to estimate |
| 5. Flat bytecode VDBE | ❌ (AST tree-walk) | very high | 2-3× |
| 6. Preallocated row scratch | ❌ | low | 10-20% |
| 7. xferOptimization (page-copy) | ❌ (batch is row-level) | medium | 10× on `INSERT ... SELECT *` |
| Schema cookie / version | ✅ Attack 3.A | — | landed |
| Plan cache by shape | ✅ Attack 2 infra | wired needs work | wired needs +1 day |
| WAL group commit | ✅ Phase 40 | — | landed |

---

## What I recommend implementing next

In priority order (highest ROI for v0.5.0-embedded-alpha):

**Attack 5 — Cursor reuse across statements (the killer feature)**
- 2-3 days. Park a B-tree cursor on `SessionContext` keyed by
  `(table_id, root_page)`. Reuse in next INSERT/UPDATE/DELETE on same
  table. Invalidate on schema_version change or root rotation.
- Estimated: **5-10× on bench INSERT**. Closes most of the SQLite gap
  on consecutive same-table writes.

**Attack 6 — Append fast path for AUTO_INCREMENT / serial PK**
- 1 day. Detect at analyze time; pass an "append hint" down through
  the executor; skip B-tree descent for monotonic keys.
- Estimated: **2-3× extra** on top of Attack 5.

**Attack 7 — USESEEKRESULT-equivalent hand-off**
- 1-2 days. Inside the clustered insert path, when constraint check
  positions the cursor, stash the position and reuse on the actual
  insert.
- Estimated: **30-50%** extra.

**Attack 8 — Row scratch buffer pool**
- 0.5-1 day. Pre-allocated `Vec<Value>` on `SessionContext`, cleared
  per row.
- Estimated: **10-20%**.

After all four: realistic projection is **closing the bench gap
from 50× to 5-10× of SQLite** — within the spec's stretch goal for
v0.5.0-embedded-alpha.

The two remaining structural items (per-page WAL framing, flat VDBE
bytecode) are deferred to post-alpha. They're the right long-term moves
but represent multi-week projects each.

---

## Sources cited

- `research/sqlite/src/insert.c` — sqlite3Insert codegen
  (lines 894, 1516, 1540, 2837, 3012)
- `research/sqlite/src/vdbe.c` — VDBE opcode handlers
  (lines 5748 OP_Insert, 5847 OP_RowCell, 280 cursor alloc)
- `research/sqlite/src/btree.c` — B-tree insert
  (lines 9394 sqlite3BtreeInsert, 9482 cursor fast path, 9656 multi-insert comment)
- `research/sqlite/src/vdbeapi.c` — sqlite3_step / sqlite3_reset
  (lines 913, 128)
- `research/sqlite/src/vdbeaux.c` — Vdbe lifecycle
  (line 2593 sqlite3VdbeRewind)
- `research/sqlite/src/wal.c` — WAL frame batching
  (line 4029 walFrames, 4266 sqlite3WalFrames)
- `research/sqlite/src/pager.c` — page cache (line 6234 sqlite3PagerWrite)
- `research/sqlite/src/prepare.c` — schema cookie check (line 518)
