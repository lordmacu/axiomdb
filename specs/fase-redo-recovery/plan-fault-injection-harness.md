# Plan: subphase 1 — fault-injection crash harness + T0

Phase: redo-recovery (project B)
Spec: specs/fase-redo-recovery/spec-redo-recovery.md
Task: Build a storage layer that simulates power-loss (only fsync'd data survives),
then T0 — a test that proves the latent durability hole (committed-but-unflushed
data is lost today, because recovery is UNDO-only). T0 is expected RED until the
REDO replay lands (subphase 4); it is the headline regression test.
Status: in progress · Branch: fase-redo-recovery · Effort: max

## Why a new storage layer (not the existing harness)

`mark_database_dirty_open` is synthetic: the `Database` drop already flushed, and
mmap `MAP_SHARED` dirty pages survive even SIGKILL (kernel page cache). So a real
power-loss — where only `fsync`'d bytes persist — cannot be reproduced with the real
mmap path in-process. We model durability explicitly instead.

## Design — `FaultInjectionStorage`

A `StorageEngine` impl (mirrors `MemoryStorage`, `crates/axiomdb-storage/src/memory.rs`)
with a two-layer page store:

```text
durable:  HashMap<u64, Page>   // only what flush() has committed
volatile: HashMap<u64, Page>   // writes since the last flush() (lost on crash)
page_count / freelist: same durable/volatile split
```

- `write_page` / `write_page_under_page_lock` → volatile overlay.
- `read_page` → `volatile.get(id).or(durable.get(id))` (current state).
- `alloc_page` / `free_page` → mutate volatile metadata (page_count, freelist bitmap).
- `flush()` → drain volatile into durable (models `fsync`/msync = "made durable").
- `simulate_power_loss()` → **drop volatile; keep durable.** Worst-case crash model
  (nothing un-fsync'd survives — the conservative, correct durability test).
- `reopen()` (or build a fresh storage from the durable snapshot) for post-crash open.

The WAL stays a **real file** with its **real fsync** (`commit_data_sync`) — that part
is already correct; we only fault-inject the *data* pages.

### Placement
`crates/axiomdb-storage/src/fault_injection.rs`, behind a `testing` cargo feature
(`pub mod fault_injection`). `axiomdb-wal` adds
`axiomdb-storage = { features = ["testing"] }` as a **dev-dependency** so recovery
tests can use it. (Keeps it out of release builds.)

## Steps

### Step 1 — `FaultInjectionStorage` + `StorageEngine` impl
Implement all trait methods (read/write/alloc/free/flush/page_count/page_lock_table/
…) over the durable+volatile split. Reuse `MemoryStorage`'s structure; add the
overlay + `flush` merge + `simulate_power_loss` + `snapshot_durable`/`from_durable`.
- Verify: `./tools/vm.sh test -p axiomdb-storage fault_injection`

### Step 2 — storage-semantics unit tests
- write→read returns the write (volatile visible).
- after `flush()`, the write is in durable; `simulate_power_loss()` keeps it.
- write WITHOUT flush, then `simulate_power_loss()` → the write is **gone**, prior
  durable state intact.
- alloc/free across flush/crash behave (page_count + freelist reconstructed from
  durable only after crash).
- Run `run_storage_engine_suite(&fault_injection_storage)` (engine.rs:192) to confirm
  it satisfies the general StorageEngine contract (pre-crash, behaves like memory).

### Step 3 — T0: prove the hole (expected RED today)
Test in `axiomdb-wal` (recovery's crate):
1. `FaultInjectionStorage` + real WAL (`TxnManager` on a tempfile).
2. Begin a txn; `write_page` a data page (volatile); record a **committed** WAL
   entry referencing it (e.g. clustered/heap insert image); `commit()` → WAL fsync.
   Do NOT `flush()` the storage (models a non-root-changing insert).
3. `simulate_power_loss()` (drop volatile) → the committed page is gone from durable.
4. Reopen storage from the durable snapshot; `CrashRecovery::recover(&mut storage,
   wal_path)`.
5. Assert the page content is restored (the committed write survived).
- **Today: FAILS** (undo-only recovery has no REDO) → proves the latent data-loss
  hole. Mark `#[ignore = "RED until REDO replay (subphase 4)"]` with a comment, OR
  keep it failing-by-design in a dedicated `redo_pending` group. Decision: `#[ignore]`
  + a sibling assertion that documents *current* (buggy) behavior so the suite stays
  green, flipped in subphase 4.

### Step 4 — close subphase
`./tools/vm.sh test -p axiomdb-storage -p axiomdb-wal`; clippy; fmt. Commit on the
branch. Update memory + the spec's crash-test plan with T0's status.

## Risk / notes
- The fault model is *worst-case* (no un-fsync'd survival) — stricter than real mmap;
  correct for durability testing (we must survive even the worst case).
- Keep `FaultInjectionStorage` behind the `testing` feature so it never ships.
- T0 staying RED is intentional and documented; subphase 4 (REDO replay) turns it
  green — that is the proof REDO works.
