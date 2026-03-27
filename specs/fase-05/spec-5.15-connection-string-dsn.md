# Spec: 5.15 — Connection string DSN

## What to build
Add a shared, typed DSN parser for AxiomDB and wire it into the two places where AxiomDB itself consumes connection/open configuration today:

- server bootstrap config;
- embedded open APIs.

This subphase is about **parsing and normalizing DSNs that AxiomDB receives**, not about adding a new outgoing client connector. The parser must accept:

- `axiomdb://user:pass@host:port/dbname?param=val`
- `mysql://...` as a compatibility alias
- `postgres://...` and `postgresql://...` as compatibility aliases
- `file:` URIs and plain local paths for embedded mode

The new behavior must:

- classify DSNs into either a **wire endpoint** or a **local embedded path**;
- percent-decode user, password, path/database name, and query parameters;
- accept bracketed IPv6 hosts;
- preserve parsed query parameters in the typed result instead of discarding them;
- let current consumers validate only the subset they actually support in `5.15`;
- keep current non-DSN entrypoints working exactly as they do today.

`5.15` does **not** add a remote client for the embedded crate, does **not** add PostgreSQL wire support, and does **not** make AxiomDB responsible for parsing ORM URLs that are already parsed by PyMySQL, SQLAlchemy, PDO, DBeaver, or pgcli themselves.

## Research synthesis

### These files were reviewed before writing this spec

#### AxiomDB
- [`db.md`](/Users/cristian/nexusdb/db.md)
- [`docs/progreso.md`](/Users/cristian/nexusdb/docs/progreso.md)
- [`docs/fase-05.md`](/Users/cristian/nexusdb/docs/fase-05.md)
- [`docs-site/src/user-guide/getting-started.md`](/Users/cristian/nexusdb/docs-site/src/user-guide/getting-started.md)
- [`docs-site/src/user-guide/embedded.md`](/Users/cristian/nexusdb/docs-site/src/user-guide/embedded.md)
- [`crates/axiomdb-server/src/main.rs`](/Users/cristian/nexusdb/crates/axiomdb-server/src/main.rs)
- [`crates/axiomdb-embedded/src/lib.rs`](/Users/cristian/nexusdb/crates/axiomdb-embedded/src/lib.rs)
- [`crates/axiomdb-cli/src/main.rs`](/Users/cristian/nexusdb/crates/axiomdb-cli/src/main.rs)
- [`crates/axiomdb-core/src/lib.rs`](/Users/cristian/nexusdb/crates/axiomdb-core/src/lib.rs)
- [`crates/axiomdb-core/src/error.rs`](/Users/cristian/nexusdb/crates/axiomdb-core/src/error.rs)
- [`crates/axiomdb-network/src/mysql/database.rs`](/Users/cristian/nexusdb/crates/axiomdb-network/src/mysql/database.rs)

#### Research
- [`research/postgres/src/interfaces/libpq/fe-connect.c`](/Users/cristian/nexusdb/research/postgres/src/interfaces/libpq/fe-connect.c)
- [`research/postgres/src/interfaces/libpq/t/001_uri.pl`](/Users/cristian/nexusdb/research/postgres/src/interfaces/libpq/t/001_uri.pl)
- [`research/postgres/src/interfaces/libpq/test/libpq_uri_regress.c`](/Users/cristian/nexusdb/research/postgres/src/interfaces/libpq/test/libpq_uri_regress.c)
- [`research/sqlite/src/main.c`](/Users/cristian/nexusdb/research/sqlite/src/main.c)
- [`research/sqlite/src/sqlite.h.in`](/Users/cristian/nexusdb/research/sqlite/src/sqlite.h.in)
- [`research/mariadb-server/include/mysql.h`](/Users/cristian/nexusdb/research/mariadb-server/include/mysql.h)
- [`research/mariadb-server/client/mysqltest.cc`](/Users/cristian/nexusdb/research/mariadb-server/client/mysqltest.cc)

### Borrow / Reject / Adapt

- PostgreSQL [`fe-connect.c`](/Users/cristian/nexusdb/research/postgres/src/interfaces/libpq/fe-connect.c) and [`001_uri.pl`](/Users/cristian/nexusdb/research/postgres/src/interfaces/libpq/t/001_uri.pl)
  Borrow: URI aliases (`postgres://` and `postgresql://`), percent-decoding, bracketed IPv6 parsing, and the separation between parsing and later consumer-specific option handling.
  Reject: libpq keyword-style `key=value` conninfo strings and the full libpq option matrix.
  Adapt: AxiomDB accepts only URI/plain-path forms in `5.15`, returns a typed parsed DSN, and leaves consumer validation to server/embedded call sites.

- SQLite [`main.c`](/Users/cristian/nexusdb/research/sqlite/src/main.c) and [`sqlite.h.in`](/Users/cristian/nexusdb/research/sqlite/src/sqlite.h.in)
  Borrow: treat parsing as a reusable primitive, preserve query parameters in the parsed result, and let later layers decide which options are meaningful.
  Reject: SQLite's VFS-specific `file:` option space (`vfs=`, `cache=`, `mode=memory`, etc.) and the idea that every URI form should automatically be acceptable to every consumer.
  Adapt: AxiomDB classifies into `Local` vs `Wire` first, then server/embedded surfaces explicitly reject unsupported params or DSN shapes.

- MariaDB [`mysql.h`](/Users/cristian/nexusdb/research/mariadb-server/include/mysql.h) and [`mysqltest.cc`](/Users/cristian/nexusdb/research/mariadb-server/client/mysqltest.cc)
  Borrow: a typed option set for host/user/password/port/timeouts/charset and explicit rejection of unsupported connection options.
  Reject: CLI-option-string parsing and the large MySQL client option surface.
  Adapt: AxiomDB normalizes URI fields into typed structs and validates a small supported subset per consumer in `5.15`.

### Design decision

The parser is a shared building block, not a client implementation. That is the crucial boundary for `5.15`.

- External MySQL/PostgreSQL tools already parse their own DSNs before talking to AxiomDB over the network.
- The new parser exists so **AxiomDB-owned surfaces** stop being path/env-only and can speak a standard DSN language too.
- Accepting `postgres://` is a parsing alias only. It does **not** imply PostgreSQL wire compatibility or a remote embedded connector in this subphase.

## Inputs / Outputs

- Input:
  - a DSN string or plain local path
  - server environment/config values
  - embedded Rust/C API open inputs
- Output:
  - typed parsed DSN:
    - `WireEndpointDsn`
    - `LocalPathDsn`
  - or a validated consumer-specific config:
    - server bootstrap config
    - embedded open path
- Errors:
  - invalid scheme
  - malformed URI
  - duplicate query parameter keys
  - unsupported DSN shape for the current consumer
  - unsupported query parameter for the current consumer

## Use cases

1. Server bootstrap with AxiomDB DSN

   `AXIOMDB_URL=axiomdb://0.0.0.0:3307/axiomdb?data_dir=/var/lib/axiomdb`

   The server binds `0.0.0.0:3307` and opens `/var/lib/axiomdb`.

2. Server bootstrap with alias DSN

   `AXIOMDB_URL=mysql://127.0.0.1:3306/axiomdb?data_dir=./data`

   The server treats this as the same wire-endpoint shape as `axiomdb://...`; the alias is only normalization, not protocol selection.

3. Embedded Rust open with local URI

   `Db::open_dsn("file:/tmp/myapp.db")`

   The embedded API opens the local database file.

4. Embedded Rust open with AxiomDB local URI

   `Db::open_dsn("axiomdb:///tmp/myapp")`

   The embedded API interprets the hostless `axiomdb:///...` form as a local database path.

5. Embedded API rejects remote wire DSN

   `Db::open_dsn("postgres://user@127.0.0.1:5432/app")`

   The parse succeeds as a wire-endpoint DSN, but the embedded consumer rejects it because AxiomDB has no remote embedded connector in `5.15`.

## Acceptance criteria

- [ ] A shared parser exists and returns a typed `LocalPathDsn` or `WireEndpointDsn` instead of raw strings.
- [ ] The parser accepts `axiomdb://`, `mysql://`, `postgres://`, and `postgresql://` URI schemes plus plain paths and `file:` URIs.
- [ ] `mysql://` and `postgres://` are aliases only; parsing them does not imply MySQL/PostgreSQL protocol changes in AxiomDB.
- [ ] Bracketed IPv6 hosts and percent-encoded credentials/path/query values are parsed correctly.
- [ ] Duplicate query parameter keys are rejected as invalid DSN input.
- [ ] The parser preserves query parameters in the typed result instead of silently discarding unknown ones.
- [ ] `axiomdb-server` accepts `AXIOMDB_URL` and derives bind host/port plus `data_dir` from it.
- [ ] Legacy server startup without `AXIOMDB_URL` keeps the current `AXIOMDB_DATA` / `AXIOMDB_PORT` behavior unchanged.
- [ ] In `5.15`, server DSN validation accepts only the subset it actually uses (`data_dir` plus endpoint fields) and rejects unsupported query params instead of silently ignoring them.
- [ ] `Db::open_dsn`, `AsyncDb::open_dsn`, and `axiomdb_open_dsn` exist and accept local-path DSNs.
- [ ] Embedded open APIs reject remote wire-endpoint DSNs with a clear DSN-specific error instead of trying to open them as file paths.
- [ ] Existing `Db::open(path)` and `axiomdb_open(path)` continue to work unchanged.

## Out of scope

- A remote client connector inside `axiomdb-embedded`
- PostgreSQL wire protocol support
- Parsing ORM-specific decorated schemes such as `mysql+pymysql://`
- `DATABASE_URL` environment-variable standardization
- JDBC-style DSNs
- Service-file or `key=value` conninfo syntax
- TLS/auth/session-option enforcement beyond current server capabilities

## Dependencies

- [`crates/axiomdb-server/src/main.rs`](/Users/cristian/nexusdb/crates/axiomdb-server/src/main.rs)
- [`crates/axiomdb-embedded/src/lib.rs`](/Users/cristian/nexusdb/crates/axiomdb-embedded/src/lib.rs)
- [`crates/axiomdb-core/src/error.rs`](/Users/cristian/nexusdb/crates/axiomdb-core/src/error.rs)
- [`docs-site/src/user-guide/getting-started.md`](/Users/cristian/nexusdb/docs-site/src/user-guide/getting-started.md)
- [`docs-site/src/user-guide/embedded.md`](/Users/cristian/nexusdb/docs-site/src/user-guide/embedded.md)

## ⚠️ DEFERRED

- `DATABASE_URL` as a standard env alias → pending in `34.5`
- remote client/open APIs that consume `WireEndpointDsn` directly → pending in a future client/tooling subphase
- broader option support (`sslmode`, pooling params, ORM-specific extras) → pending in a later DSN/tooling phase once there is a real consumer
