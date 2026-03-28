# Spec: 3.19d — Configurable WAL durability policy

## What to build (not how)

Introduce an explicit WAL durability policy for committed DML so AxiomDB can
separate:

- the **sync method** used by the WAL hot path (`3.19c`);
- the **durability contract** exposed to the caller on commit.

Today, AxiomDB effectively has two implicit durability modes:

- `fsync = true` → strict durable commit before `OK`
- `fsync = false` → unsafe benchmark/dev mode

This subphase must replace that implicit boolean with an explicit,
documented policy while preserving the current safe default.

The new policy must:

- keep `strict` as the default and preserve today's safe behavior;
- add opt-in relaxed modes for benchmark/dev workloads where per-commit
  durability is too expensive;
- keep `3.19b` WAL reserved-tail semantics unchanged;
- keep `3.19c` sync-method selection unchanged;
- make the durability trade-off visible in code, config, docs, and tests;
- remain compatible with the `6.19` fsync pipeline, but without pretending
  that leader-based batching alone fixes single-connection request/response
  autocommit workloads.

## Research synthesis

### AxiomDB files reviewed first

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-storage/src/config.rs`
- `crates/axiomdb-storage/src/mmap.rs`
- `crates/axiomdb-wal/src/txn.rs`
- `crates/axiomdb-wal/src/writer.rs`
- `crates/axiomdb-wal/src/fsync_pipeline.rs`
- `crates/axiomdb-network/src/mysql/database.rs`
- `specs/fase-03/spec-3.19b-wal-durable-fast-path.md`
- `specs/fase-03/spec-3.19c-wal-sync-policy.md`

### Research sources and what to borrow

- `research/postgres/src/backend/access/transam/xact.c`
  Borrow: durability acknowledgment policy (`synchronous_commit`) is distinct
  from the low-level sync method.
  Reject: PostgreSQL's full WAL writer/checkpointer/buffer-manager stack.
  Adapt: AxiomDB needs a small explicit durability enum, not a larger
  transaction subsystem rewrite.

- `research/postgres/src/backend/access/transam/xlog.c`
  Borrow: keep sync method (`wal_sync_method`) separate from commit semantics.
  Reject: treating syscall selection as enough to solve all commit throughput
  issues.
  Adapt: `3.19c` already solved method selection; `3.19d` must sit above it.

- `research/mariadb-server/storage/innobase/srv/srv0srv.cc`
  Borrow: `innodb_flush_log_at_trx_commit` exposes durability/performance as an
  explicit policy knob.
  Reject: InnoDB's broader redo/log subsystem and background flush machinery.
  Adapt: AxiomDB can expose strict vs relaxed commit durability without copying
  InnoDB internals.

- `research/mariadb-server/mysys/my_sync.c`
  Borrow: method selection and durability contract are separate concerns.
  Reject: hiding unsafe behavior behind implicit fallbacks.
  Adapt: AxiomDB must document exactly what each policy guarantees.

- `research/sqlite/src/pager.h`
  Borrow: `PRAGMA synchronous = FULL | NORMAL | OFF` is a clear model for
  naming durability modes.
  Reject: SQLite's pager and checkpoint-driven WAL semantics as an
  implementation template.
  Adapt: AxiomDB can use similarly explicit policy names while keeping its own
  WAL + mmap architecture.

- `research/sqlite/src/os_unix.c`
  Borrow: durability/performance trade-offs are platform-sensitive and should
  be explicit.
  Reject: assuming that SQLite's `NORMAL` semantics can be copied 1:1 onto an
  mmap-based engine.
  Adapt: AxiomDB must document that relaxed modes are benchmark/dev-oriented
  and do not provide the same crash guarantees as `strict`.

## Inputs / Outputs

- Input:
  - `DbConfig` in `crates/axiomdb-storage/src/config.rs`
  - `TxnManager::commit()` in `crates/axiomdb-wal/src/txn.rs`
  - server/embedded open paths that construct the runtime config
  - `6.19` fsync-pipeline callers
- Output:
  - an explicit durability policy for DML commits, configurable at open time;
  - default safe behavior remains unchanged;
  - relaxed modes are opt-in, documented, and benchmarkable.
- Errors:
  - invalid durability-policy values fail config parsing/open clearly;
  - unsupported combinations must fail explicitly instead of silently
    downgrading or upgrading guarantees.

## Use cases

1. Default production open.
   - Policy = `strict`
   - A committed DML statement is acknowledged only after WAL durability is
     confirmed, exactly as today.

2. Benchmark/dev open on Linux/macOS.
   - Policy = `normal`
   - The server may acknowledge a committed DML statement after WAL bytes are
     flushed to the OS, without waiting for durable sync on every commit.
   - Throughput improves, but acknowledged commits may be lost after crash or
     power loss.

3. Fully relaxed local benchmark mode.
   - Policy = `off`
   - The engine may acknowledge commits without durable sync and without a
     per-commit WAL flush barrier.
   - Intended only for development/bench, never as the default.

4. Existing config using `fsync = false`.
   - Backward compatibility maps it to the explicit relaxed durability mode
     chosen by the final design.
   - The old boolean is no longer the source of truth.

## Acceptance criteria

- [ ] `strict` remains the default and preserves today's durable-commit
      semantics exactly.
- [ ] A new explicit durability-policy enum/config exists in code instead of
      overloading `fsync: bool` for both sync method and commit semantics.
- [ ] `normal` and/or `off` are opt-in only and clearly documented as relaxed
      durability modes.
- [ ] `3.19c` sync-method selection remains orthogonal to the durability
      policy.
- [ ] `6.19` remains compatible with the new policy: `strict` still waits for
      durability before visibility/ack; relaxed modes follow their documented
      contract consistently.
- [ ] Targeted tests cover all policy modes and configuration parsing.
- [ ] `local_bench.py --scenario insert_autocommit --rows 1000 --table --engines axiomdb`
      shows a measurable improvement in the relaxed mode over `strict` on the
      same machine and release build.

## Out of scope

- Making relaxed durability the default
- Rewriting the mmap storage engine or adding a full buffer pool
- Reworking the MySQL wire protocol to pipeline commands from sequential clients
- Full WAL writer/background flusher architecture like PostgreSQL/InnoDB
- SQL/session variable exposure of the durability policy

## Dependencies

- `3.16` — configuration loading
- `3.19b` — WAL reserved-tail fast path
- `3.19c` — explicit sync-method selection
- `6.19` — server fsync-pipeline integration

## ⚠️ DEFERRED

- SQL/session-level durability toggles
  → pending in a future config/session subphase
- Any stronger guarantee for relaxed modes on mmap-backed storage
  → pending in a future storage-architecture subphase if AxiomDB moves away
    from relying on OS-managed dirty mmap flushing
