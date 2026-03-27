# Plan: 5.15 — Connection string DSN

## Files to create/modify

- [`Cargo.toml`](/Users/cristian/nexusdb/Cargo.toml) — add the shared URI parsing dependency used by the DSN module
- [`crates/axiomdb-core/Cargo.toml`](/Users/cristian/nexusdb/crates/axiomdb-core/Cargo.toml) — enable the DSN parser dependency for the shared crate
- [`crates/axiomdb-core/src/dsn.rs`](/Users/cristian/nexusdb/crates/axiomdb-core/src/dsn.rs) — new shared typed parser and DSN classification logic
- [`crates/axiomdb-core/src/error.rs`](/Users/cristian/nexusdb/crates/axiomdb-core/src/error.rs) — add a DSN-specific error variant
- [`crates/axiomdb-core/src/lib.rs`](/Users/cristian/nexusdb/crates/axiomdb-core/src/lib.rs) — export the DSN parser/types
- [`crates/axiomdb-server/src/main.rs`](/Users/cristian/nexusdb/crates/axiomdb-server/src/main.rs) — replace ad hoc env parsing with a typed bootstrap-config helper that understands `AXIOMDB_URL`
- [`crates/axiomdb-embedded/src/lib.rs`](/Users/cristian/nexusdb/crates/axiomdb-embedded/src/lib.rs) — add `Db::open_dsn`, `AsyncDb::open_dsn`, and `axiomdb_open_dsn`
- [`crates/axiomdb-embedded/tests/integration.rs`](/Users/cristian/nexusdb/crates/axiomdb-embedded/tests/integration.rs) — local DSN open tests plus remote-DSN rejection tests
- [`docs-site/src/user-guide/getting-started.md`](/Users/cristian/nexusdb/docs-site/src/user-guide/getting-started.md) — server-side DSN usage docs
- [`docs-site/src/user-guide/embedded.md`](/Users/cristian/nexusdb/docs-site/src/user-guide/embedded.md) — embedded `open_dsn` docs

## Algorithm / Data structure

### 1. Shared parsed representation

Add a shared typed result in `axiomdb-core`:

```text
enum ParsedDsn {
  Local(LocalPathDsn),
  Wire(WireEndpointDsn),
}

struct LocalPathDsn {
  original_scheme: LocalScheme,   // PlainPath | File | AxiomDbLocal
  path: PathBuf,
  query: BTreeMap<String, String>,
}

struct WireEndpointDsn {
  original_scheme: WireScheme,    // AxiomDb | MySql | Postgres
  user: Option<String>,
  password: Option<String>,
  host: String,
  port: Option<u16>,
  database: Option<String>,
  query: BTreeMap<String, String>,
}
```

Rules:

1. Plain strings without a URI scheme are `Local`.
2. `file:` URIs are `Local`.
3. `mysql://`, `postgres://`, and `postgresql://` are always `Wire`.
4. `axiomdb://` is:
   - `Local` only when the URI has empty authority and only a path (`axiomdb:///tmp/app.db`);
   - `Wire` when it has a host/authority.
5. Percent-decode user/password/database/path/query values.
6. Accept bracketed IPv6 addresses.
7. Reject duplicate query keys during parse to avoid consumer ambiguity.

### 2. Parse once, validate per consumer

The parser should be permissive about *syntax* but not about ambiguous structure.
Consumer layers then validate supported semantics.

This is the core design choice borrowed from PostgreSQL and SQLite:

- parser normalizes and preserves information;
- consumers decide which fields/query params are legal for that specific surface.

### 3. Server bootstrap consumer

Extract current startup env logic from `main.rs` into a helper like:

```text
ServerBootstrapConfig::from_env()
```

Behavior:

1. If `AXIOMDB_URL` is absent:
   - keep current behavior:
     - `AXIOMDB_DATA` fallback to `./data`
     - `AXIOMDB_PORT` fallback to `3306`
     - bind host stays `0.0.0.0`
2. If `AXIOMDB_URL` is present:
   - parse with `parse_dsn(...)`
   - require `ParsedDsn::Wire`
   - bind host = parsed host
   - bind port = parsed port or `3306`
   - `data_dir` = query param `data_dir`, else `AXIOMDB_DATA`, else `./data`
   - reject any query params other than `data_dir`
3. Parsed `user`, `password`, and `database` are accepted syntactically but are not used to configure server auth/session defaults in `5.15`.

This must be explicit in code/docs so the implementation does not silently invent semantics for those fields.

### 4. Embedded consumer

Add:

```text
Db::open_dsn(dsn: impl AsRef<str>) -> Result<Self, DbError>
AsyncDb::open_dsn(dsn: impl Into<String>) -> Result<Self, DbError>
axiomdb_open_dsn(const char*) -> *mut Db
```

Behavior:

1. Parse DSN.
2. Require `ParsedDsn::Local`.
3. In `5.15`, reject all query params for embedded open.
4. Call the existing `Db::open(path)` implementation with the resolved local path.
5. If the parsed result is `ParsedDsn::Wire`, return a DSN-specific error:
   remote endpoint DSNs are not supported by the embedded API in this subphase.

### 5. Error contract

Add a dedicated core error variant instead of overloading generic value errors:

```text
DbError::InvalidDsn { reason: String }
```

Use it for:

- malformed URI
- unsupported scheme
- duplicate query params
- unsupported DSN shape for a consumer
- unsupported query params for a consumer

This keeps failures visible and documentation-friendly.

## Implementation phases

1. Add shared DSN types and parser in `axiomdb-core`.
2. Add DSN-specific error variant and export surface.
3. Extract server bootstrap config parsing from `main.rs` and wire in `AXIOMDB_URL`.
4. Add embedded Rust `open_dsn` APIs.
5. Add C FFI `axiomdb_open_dsn`.
6. Add async wrapper `AsyncDb::open_dsn`.
7. Add parser unit tests.
8. Add embedded integration tests and server bootstrap tests.

## Tests to write

- unit:
  - `mysql://` parses as `Wire`
  - `postgres://` and `postgresql://` parse as `Wire`
  - `axiomdb:///tmp/app.db` parses as `Local`
  - bracketed IPv6 parses correctly
  - percent-decoding works for username/password/path/query
  - duplicate query params fail with `InvalidDsn`
- integration:
  - `Db::open_dsn("file:/tmp/...")` opens a real DB and persists data
  - `Db::open_dsn("axiomdb:///tmp/...")` opens a real DB and persists data
  - `Db::open_dsn("postgres://...")` fails with `InvalidDsn`
  - `axiomdb_open_dsn(...)` works for local DSNs
  - server bootstrap with `AXIOMDB_URL=axiomdb://127.0.0.1:3307/db?data_dir=...` produces the expected host/port/data_dir config
  - server bootstrap rejects unsupported query params
- bench:
  - none required; this subphase is config/tooling, not a performance path

## Anti-patterns to avoid

- Do not build a remote client connector inside `axiomdb-embedded`.
- Do not interpret `postgres://` as “AxiomDB now supports PostgreSQL wire”.
- Do not silently ignore unsupported query params at the consumer layer.
- Do not put the parser in `axiomdb-network`; DSN parsing is shared config logic, not a wire-protocol concern.
- Do not break existing path-based open/startup flows while adding DSN support.

## Risks

- Risk: ambiguous interpretation of `axiomdb:///path` vs `axiomdb://host/db`.
  Mitigation: classify `axiomdb:///...` with empty authority as `Local`, and any authority-bearing form as `Wire`.

- Risk: server config silently suggests that user/password/dbname affect authentication or default session state.
  Mitigation: keep those fields parsed but unused in `5.15`, and document that clearly.

- Risk: percent-decoding or IPv6 parsing bugs make the parser subtly incompatible with standard tooling.
  Mitigation: use a standards-compliant URI parser for URI forms and back it with parser tests modeled on libpq-style edge cases.

- Risk: consumer-specific validation becomes inconsistent across server and embedded.
  Mitigation: keep one shared parser and separate, explicit validation helpers per consumer.

## Assumptions

- `5.15` is a parsing/configuration subphase, not a network protocol subphase.
- Existing ORMs and MySQL drivers continue to parse their own URLs when they connect over AxiomDB's MySQL wire protocol; this subphase does not replace that.
- `DATABASE_URL` remains deferred to `34.5`, so `5.15` only introduces `AXIOMDB_URL`.
