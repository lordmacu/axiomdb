# Plan: 3.19d — Configurable WAL durability policy

## Files to create/modify

- `crates/axiomdb-storage/src/config.rs` — add explicit durability-policy enum
  and config parsing; keep legacy `fsync` compatibility path
- `crates/axiomdb-wal/src/txn.rs` — route DML commit behavior by policy
- `crates/axiomdb-wal/src/fsync_pipeline.rs` — preserve strict-mode semantics
  and document relaxed-mode interactions
- `crates/axiomdb-wal/src/lib.rs` — export the durability-policy type only if
  needed outside the crate
- `crates/axiomdb-network/src/mysql/database.rs` — wire runtime config into the
  TxnManager/server open path
- `crates/axiomdb-embedded/src/lib.rs` — same for embedded open path if config
  is consumed there
- `crates/axiomdb-wal/tests/integration_durability.rs` — policy-mode crash/open
  coverage
- `crates/axiomdb-network/tests/` and `tools/wire-test.py` — strict-vs-relaxed
  observable behavior only if the policy is wire-visible at config level

## Algorithm / Data structure

### 1. Add a durability-policy enum above the sync method

Conceptually:

```text
WalDurabilityPolicy {
  Strict,
  Normal,
  Off,
}
```

Rules:

- `Strict`
  - current behavior
  - no `OK` before durable WAL sync
- `Normal`
  - WAL bytes are flushed to the OS before `OK`
  - no durable sync on every commit
  - acknowledged commits may be lost after crash/power loss
- `Off`
  - no per-commit durability barrier
  - benchmark/dev only

`WalSyncMethod` from `3.19c` stays separate:

```text
WalSyncMethod -> which syscall to use when syncing
WalDurabilityPolicy -> whether a commit waits for sync at all
```

### 2. Preserve the current default

Config resolution:

```text
if explicit wal_durability present:
  use it
else if legacy fsync == false:
  map to Off (or chosen relaxed mode)
else:
  Strict
```

This keeps old configs working while making the new policy the source of truth.

### 3. Route `TxnManager::commit()` by policy

Pseudocode:

```text
commit():
  append Commit entry

  if read-only:
    wal.flush_no_sync()
    max_committed = txn_id
    return

  match wal_durability:
    Strict:
      if deferred_commit_mode:
        pending_deferred_txn_id = txn_id
      else:
        wal.commit_data_sync()
        max_committed = txn_id

    Normal:
      wal.flush_no_sync()
      max_committed = txn_id

    Off:
      // minimum barrier selected by final implementation
      max_committed = txn_id
```

Important:

- `Strict` remains the only fully crash-safe mode.
- Relaxed modes intentionally trade durability for throughput.
- The exact `Off` barrier can still be conservative (`flush_no_sync`) if the
  implementation wants to avoid a fully unflushed BufWriter acknowledgment.

### 4. Define `6.19` interaction explicitly

Pseudocode:

```text
if policy == Strict:
  existing pipeline semantics unchanged
else:
  bypass pipeline and use relaxed commit path directly
```

Reason:

- the pipeline is only meaningful when waiting for durable sync is part of the
  contract;
- in relaxed modes, pretending to pipeline a sync that the commit does not wait
  for only adds complexity and ambiguity.

### 5. Validate with isolated benchmarks

Do not rely only on the full multi-engine Docker suite for acceptance.

Use:

- isolated `axiomdb` harness for `insert_autocommit`
- same release build
- compare `strict` vs `normal` vs `off`

## Implementation phases

1. Add `WalDurabilityPolicy` to config/runtime resolution with backward
   compatibility for `fsync`.
2. Thread the resolved policy into `TxnManager` construction/open.
3. Route `TxnManager::commit()` and `wal_flush_and_fsync()` behavior by policy.
4. Define and implement `6.19` behavior under relaxed modes.
5. Add targeted tests and isolated benchmark harness coverage.

## Tests to write

- unit:
  - config parsing for `strict`, `normal`, `off`
  - legacy `fsync=false` compatibility mapping
  - invalid policy value rejected
- integration:
  - `strict` keeps current crash-safe semantics
  - relaxed mode reopens cleanly after normal shutdown
  - relaxed mode behavior is explicitly documented in crash/recovery tests
  - `6.19` strict-mode behavior remains unchanged
- bench:
  - isolated `insert_autocommit` on Linux/macOS with `strict`
  - same benchmark with `normal`
  - optional `off` benchmark for upper bound

## Anti-patterns to avoid

- Do not silently change the default away from `strict`.
- Do not overload `WalSyncMethod` to also mean acknowledgment policy.
- Do not pretend relaxed modes are crash-safe.
- Do not judge acceptance only from the noisy multi-engine Docker suite.
- Do not try to "fix" single-client autocommit by adding more leader batching;
  the protocol arrival pattern does not support it.

## Risks

- Risk: relaxed durability is misunderstood as still fully safe.
  - Mitigation: explicit naming, explicit docs, and tests that keep `strict`
    as the default production path.

- Risk: `fsync` legacy config and new durability policy conflict.
  - Mitigation: define one precedence order and keep only one source of truth
    after parsing.

- Risk: relaxed-mode semantics interact ambiguously with the `6.19` pipeline.
  - Mitigation: bypass the pipeline in relaxed modes and keep it only for
    `strict`.

- Risk: mmap-backed storage makes relaxed modes weaker than PostgreSQL/SQLite.
  - Mitigation: document that clearly; do not overclaim crash guarantees that
    the current storage architecture cannot provide.
