# Spec: 5.2a — Charset/collation negotiation in handshake

## What to build (not how)

AxiomDB Phase 5.2 already parses the client's `character_set` byte from
`HandshakeResponse41`, but the current server still behaves as UTF-8-only:

- the handshake value is ignored after auth
- session state only stores one string field, `character_set_client`
- `COM_QUERY`, `COM_STMT_PREPARE`, and `COM_INIT_DB` decode bytes with
  `std::str::from_utf8(...)`
- `COM_STMT_EXECUTE` string parameters use `String::from_utf8_lossy(...)`
- outgoing rows, column names, and column definitions are always emitted as
  UTF-8 with collation id `255`

`5.2a` must make charset negotiation real at the MySQL wire boundary without
pretending that AxiomDB already has full SQL collation semantics internally.

This subphase adds a transport-charset layer for the current codebase:

1. the handshake-selected collation initializes the connection session state
2. inbound client text is decoded according to the negotiated client charset
3. inbound prepared-statement string parameters are decoded with that same
   client charset
4. outbound text rows and metadata are encoded according to the session result
   charset/collation
5. session variables exposed through `SELECT @@...` and `SHOW VARIABLES`
   reflect the real negotiated state instead of hardcoded UTF-8 placeholders

### Supported charsets and collations in 5.2a

`5.2a` is intentionally a supported subset, not the full MySQL/MariaDB matrix.
The supported transport surface is:

| Charset | Accepted names | Supported collations | Notes |
|---|---|---|---|
| `utf8mb4` | `utf8mb4` | `utf8mb4_0900_ai_ci` (`255`), `utf8mb4_general_ci` (`45`), `utf8mb4_bin` (`46`) | server default is `utf8mb4_0900_ai_ci` |
| `utf8mb3` | `utf8mb3`, `utf8` | `utf8mb3_general_ci` (`33`), `utf8mb3_bin` (`83`) | `utf8` is accepted as an alias and normalized to `utf8mb3` |
| `latin1` | `latin1` | `latin1_swedish_ci` (`8`), `latin1_bin` (`47`) | MySQL/MariaDB `latin1` means cp1252-style transport, not strict ISO-8859-1 |
| metadata-only | — | `binary` (`63`) | used for BLOB/bytes and non-text metadata fields |

Everything outside that table is rejected clearly instead of being accepted and
silently mis-decoded.

### Scope boundaries

This subphase is about transport encoding and session negotiation only.

It does **not** claim that AxiomDB now supports:

- locale-aware SQL comparison semantics
- per-expression or per-column collation derivation in the analyzer/executor
- index semantics that depend on collation
- full MySQL charset catalog compatibility

Those remain later work. `5.2a` is specifically about making the current MySQL
wire path honest and correct.

### Handshake/session semantics

- the server greeting continues to advertise server default collation id `255`
  (`utf8mb4_0900_ai_ci`)
- the client's handshake `character_set` byte selects the initial session
  charset/collation if the collation id is in the supported table above
- unsupported handshake charset/collation ids are rejected before auth success
  and the connection closes
- after a successful handshake:
  - `@@character_set_client` = negotiated charset
  - `@@character_set_connection` = negotiated charset
  - `@@character_set_results` = negotiated charset
  - `@@collation_connection` = negotiated collation
  - `@@character_set_server` and `@@collation_server` remain the AxiomDB
    server defaults
- `COM_RESET_CONNECTION` resets charset/collation state to the server defaults,
  not to the original client handshake choice

### SET semantics

`5.2a` must make the existing intercepted `SET` surface coherent:

- `SET NAMES <charset>`
  - sets client, connection, and results charsets together
  - sets connection/result collation to that charset's default collation
- `SET NAMES <charset> COLLATE <collation>`
  - sets client, connection, and results charsets together
  - sets both connection/result collation to the explicitly chosen collation
- `SET character_set_client = <charset>`
  - changes only the client-side decode charset
- `SET character_set_connection = <charset>`
  - changes only the connection charset and resets
    `collation_connection` to that charset's default collation
- `SET character_set_results = <charset>`
  - changes only outbound text encoding
  - resets the internal result-metadata collation to that charset's default
    collation
- `SET collation_connection = <collation>`
  - changes only the connection collation and the derived
    `character_set_connection`
  - does not change `character_set_client`
  - does not change `character_set_results`
- invalid charset or collation names return ERR and leave the previous session
  charset state unchanged
- incompatible `SET NAMES <charset> COLLATE <collation>` pairs return ERR and
  leave the previous session charset state unchanged

### Input decoding semantics

The negotiated client charset must be applied to all inbound text surfaces that
exist in the current codebase:

- handshake `username`
- handshake optional `database`
- `COM_INIT_DB` database name
- `COM_QUERY` SQL text
- `COM_STMT_PREPARE` SQL text
- `COM_STMT_EXECUTE` string-like parameters (`STRING`, `VAR_STRING`, BLOB-ish
  string payloads, `DECIMAL` textual payloads)

Important rules:

- `utf8mb4` uses normal UTF-8 validation/decoding
- `utf8mb3` uses UTF-8 byte decoding but rejects 4-byte code points
- `latin1` must use cp1252-compatible decoding
- inbound decode must not use lossy replacement

### Output encoding semantics

The negotiated result charset must be applied to all outbound textual surfaces
that exist in the current codebase:

- text protocol row cells for `TEXT`, `DECIMAL`, and `UUID` values
- binary protocol row cells for text-like values (`TEXT`, `DECIMAL`, `UUID`)
- column names in result-set metadata
- text-like column-definition charset ids
- intercepted rowsets such as `SHOW VARIABLES`, `SHOW STATUS`, and
  `SELECT @@var`

Important rules:

- `BLOB` / `Bytes` values remain raw bytes and are never transcoded
- text-like result metadata uses the current result collation id
- `BLOB` and non-text metadata fields use binary collation id `63`
- outbound encode must not use lossy replacement
- if a result string cannot be represented in the chosen outbound charset,
  the statement returns ERR through the existing query-error path

### Research synthesis

AxiomDB-first constraints:

- `crates/axiomdb-network/src/mysql/packets.rs` already parses the handshake
  charset byte, but only as a raw `u8`
- `crates/axiomdb-network/src/mysql/handler.rs` currently ignores that byte and
  hardcodes UTF-8 text decoding
- `crates/axiomdb-network/src/mysql/session.rs` currently collapses all charset
  variables into one string field
- `crates/axiomdb-network/src/mysql/prepared.rs` currently decodes string
  parameters with UTF-8 lossy conversion
- `crates/axiomdb-network/src/mysql/result.rs` currently emits all text as
  UTF-8 with hardcoded collation id `255`

What was borrowed from research:

- MariaDB charset metadata and session variable behavior:
  - `research/mariadb-server/sql/share/charsets/Index.xml`
  - `research/mariadb-server/strings/ctype-utf8.c`
  - `research/mariadb-server/mysql-test/main/ctype_collate.test`
  - `research/mariadb-server/mysql-test/main/ctype_collate.result`
  - `research/mariadb-server/sql/share/errmsg-utf8.txt`
- PostgreSQL's discipline of validating/transforming text at the
  client/server boundary, not inside the parser or value system:
  - `research/postgres/src/backend/commands/variable.c`
  - `research/postgres/src/backend/utils/mb/mbutils.c`
- OceanBase's separation of session charset fields instead of storing one
  stringly-typed value:
  - `research/oceanbase/src/sql/session/ob_basic_session_info.h`
- SQLite as a design reminder that coercion/validation should live at the
  boundary where data crosses layers, not inside generic scalar containers:
  - `research/sqlite/src/insert.c`

What AxiomDB borrows:

- MariaDB/MySQL-compatible transport names, ids, and `SET NAMES` surface
- PostgreSQL's fast-path idea: when the selected encoding already matches the
  internal representation, avoid unnecessary conversion but still validate
  input
- OceanBase's structured session state with separate client, connection, and
  results fields

What AxiomDB rejects:

- copying MariaDB's full charset catalog or collation engine into `5.2a`
- pushing charset conversion down into `axiomdb-types`, the SQL parser, or the
  executor
- lossy replacement on decode/encode

How AxiomDB adapts it:

- AxiomDB stays UTF-8 internally
- only the MySQL network boundary gains transport transcoding
- the supported subset is deliberately small, explicit, and testable

## Inputs / Outputs

- Input:
  - the handshake `character_set` byte in `HandshakeResponse41`
  - inbound command bytes from `COM_QUERY`, `COM_INIT_DB`,
    `COM_STMT_PREPARE`, and `COM_STMT_EXECUTE`
  - intercepted `SET` statements that modify session charset state
  - outbound `QueryResult` values and metadata serialized by the MySQL wire
    layer
- Output:
  - a connection session initialized from the negotiated charset/collation
  - correctly decoded inbound Unicode text for the existing SQL stack
  - correctly encoded outbound row/metadata bytes for the selected result
    charset
  - accurate `@@character_set_*` and `@@collation_*` session variables
- Errors:
  - unsupported handshake collation id → ERR before auth success, then close
  - unsupported `SET` charset/collation → ERR, previous state preserved
  - invalid client bytes for the negotiated charset → ERR through the existing
    query parse/error path
  - outbound text not representable in the selected result charset → ERR
    through the existing query-error path

## Use cases

1. A default client handshake using collation id `255` initializes the session
   as `utf8mb4 / utf8mb4_0900_ai_ci`.
2. A client handshake using collation id `8` initializes the session as
   `latin1 / latin1_swedish_ci`.
3. `SET NAMES latin1` changes all three `character_set_*` variables to
   `latin1` and `@@collation_connection` to `latin1_swedish_ci`.
4. `SET NAMES latin1 COLLATE latin1_bin` changes session metadata to the binary
   latin1 collation without pretending that engine-wide SQL collation semantics
   are implemented.
5. `SET character_set_results = utf8mb3` changes outbound text encoding but
   leaves `character_set_client` unchanged.
6. A `COM_QUERY` sent in latin1 bytes containing `café` is decoded correctly by
   AxiomDB before parsing/analyzing SQL.
7. A `COM_STMT_EXECUTE` string parameter sent as latin1/cp1252 bytes is decoded
   correctly instead of using UTF-8 lossy replacement.
8. A result row containing `€` and sent to a latin1 client is encoded as the
   cp1252 byte `0x80`, matching MySQL/MariaDB `latin1` behavior.
9. A result row containing an emoji and sent to a latin1 client returns ERR
   instead of silently replacing the character.
10. A client using `SET NAMES utf8` is accepted, normalized to `utf8mb3`, and
    outbound text rejects 4-byte code points that are not representable in
    utf8mb3.
11. `SHOW VARIABLES LIKE 'character_set_%'` reflects the actual session state
    instead of mirroring `character_set_client` into every field.
12. `COM_RESET_CONNECTION` restores the connection to the server-default
    `utf8mb4 / utf8mb4_0900_ai_ci` state.

## Acceptance criteria

- [ ] the handshake `character_set` byte is no longer ignored after auth
- [ ] the initial `ConnectionState` charset/collation comes from the negotiated
      handshake collation when that collation id is supported
- [ ] unsupported handshake collation ids are rejected before auth success and
      the connection closes
- [ ] `COM_QUERY` uses negotiated client charset decoding instead of hardcoded
      UTF-8 decoding
- [ ] `COM_STMT_PREPARE` uses negotiated client charset decoding instead of
      hardcoded UTF-8 decoding
- [ ] `COM_INIT_DB` uses negotiated client charset decoding instead of
      `String::from_utf8_lossy`
- [ ] `COM_STMT_EXECUTE` string-like parameters are decoded with the negotiated
      client charset instead of `String::from_utf8_lossy`
- [ ] `latin1` transport uses cp1252-compatible conversion, not strict
      ISO-8859-1 semantics
- [ ] `utf8mb3` accepts 1–3 byte UTF-8 text and rejects 4-byte code points
- [ ] `SET NAMES <charset>` updates `character_set_client`,
      `character_set_connection`, `character_set_results`, and
      `collation_connection` coherently
- [ ] `SET NAMES <charset> COLLATE <collation>` updates the connection state
      coherently when the pair is supported
- [ ] invalid `SET NAMES` charset names return ERR and preserve the previous
      charset state
- [ ] invalid `SET NAMES ... COLLATE ...` combinations return ERR and preserve
      the previous charset state
- [ ] `SET character_set_client`, `SET character_set_connection`,
      `SET character_set_results`, and `SET collation_connection` each update
      only the state they are supposed to change
- [ ] `SELECT @@character_set_client`, `@@character_set_connection`,
      `@@character_set_results`, and `@@collation_connection` reflect the real
      session state
- [ ] `SHOW VARIABLES` reflects distinct client/connection/results values
      instead of mirroring one field into all charset variables
- [ ] text protocol row values are encoded with the selected result charset
- [ ] binary protocol text-like row values are encoded with the selected result
      charset
- [ ] column names in result metadata are encoded with the selected result
      charset
- [ ] text-like column definitions advertise the selected result collation id
- [ ] BLOB / bytes column definitions use binary collation id `63`
- [ ] outbound text that is not representable in the selected result charset
      returns ERR instead of lossy replacement
- [ ] `COM_RESET_CONNECTION` restores the server-default session charset state
- [ ] regression coverage includes: default utf8mb4 handshake, latin1
      handshake, `SET NAMES`, prepared statement string params, outbound latin1
      encoding, utf8mb3 4-byte rejection, and reset-to-default behavior

## Out of scope

- full SQL collation semantics in `ORDER BY`, `GROUP BY`, joins, indexes,
  constraints, or expression evaluation
- per-table or per-database default charset persistence
- `SET CHARACTER SET ...` compatibility semantics
- `character_set_results = NULL` or `character_set_results = binary`
- full MySQL/MariaDB charset catalog coverage beyond the supported subset above
- transcoding of ERR packet messages or OK-packet info strings
- collations outside:
  - `utf8mb4_0900_ai_ci`
  - `utf8mb4_general_ci`
  - `utf8mb4_bin`
  - `utf8mb3_general_ci`
  - `utf8mb3_bin`
  - `latin1_swedish_ci`
  - `latin1_bin`

## Dependencies

These files were reviewed before writing this spec:

- `db.md`
- `docs/progreso.md`
- `crates/axiomdb-network/Cargo.toml`
- `crates/axiomdb-network/src/mysql/mod.rs`
- `crates/axiomdb-network/src/mysql/packets.rs`
- `crates/axiomdb-network/src/mysql/handler.rs`
- `crates/axiomdb-network/src/mysql/session.rs`
- `crates/axiomdb-network/src/mysql/result.rs`
- `crates/axiomdb-network/src/mysql/prepared.rs`
- `crates/axiomdb-network/tests/integration_protocol.rs`
- `tools/wire-test.py`
- `specs/fase-05/spec-5.1-5.7-critical-path.md`
- `specs/fase-05/spec-5.3b-5.8-5.9.md`

Research reviewed before writing this spec:

- `research/mariadb-server/sql/share/charsets/Index.xml`
- `research/mariadb-server/strings/ctype-utf8.c`
- `research/mariadb-server/mysql-test/main/ctype_collate.test`
- `research/mariadb-server/mysql-test/main/ctype_collate.result`
- `research/mariadb-server/sql/share/errmsg-utf8.txt`
- `research/postgres/src/backend/commands/variable.c`
- `research/postgres/src/backend/utils/mb/mbutils.c`
- `research/oceanbase/src/sql/session/ob_basic_session_info.h`
- `research/sqlite/src/insert.c`

## ⚠️ DEFERRED

- full SQL collation semantics for comparison, sorting, grouping, and index
  behavior → pending `5.2b`
- `SET CHARACTER SET` MySQL compatibility semantics → pending `5.2b`
- additional MySQL/MariaDB collations beyond the supported subset above
  → pending `5.2b`
- transcoding of ERR packet messages and OK-packet info strings
  → pending later Phase 5 compatibility work
