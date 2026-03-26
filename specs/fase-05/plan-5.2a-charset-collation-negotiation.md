# Plan: 5.2a — Charset/collation negotiation in handshake

## Files to create/modify

- `crates/axiomdb-network/Cargo.toml`
  - add `encoding_rs = "0.8"` for legacy charset conversion
- `crates/axiomdb-network/src/mysql/mod.rs`
  - export a new `charset` module
- `crates/axiomdb-network/src/mysql/charset.rs`
  - add the supported transport charset/collation registry
  - implement decode/encode helpers with validation and non-lossy behavior
  - centralize charset aliases and collation ids
- `crates/axiomdb-network/src/mysql/session.rs`
  - replace the one-field charset model with typed session charset state
  - implement `SET NAMES` and individual charset/collation setters against the
    registry
  - expose getters used by `@@var` and `SHOW VARIABLES`
- `crates/axiomdb-network/src/mysql/packets.rs`
  - stop decoding handshake `username` / `database` as UTF-8 strings inside the
    packet parser
  - return raw bytes for handshake fields so `handler.rs` can decode them with
    the negotiated charset
  - replace the hardcoded greeting charset literal with the registry constant
- `crates/axiomdb-network/src/mysql/handler.rs`
  - initialize `ConnectionState` from the handshake collation id
  - decode handshake/database/query text through session charset helpers
  - thread result-collation context into all serializer call sites
  - handle serializer failures as query ERR packets
- `crates/axiomdb-network/src/mysql/prepared.rs`
  - decode string-like `COM_STMT_EXECUTE` params with negotiated client charset
  - thread result-collation context into `COM_STMT_PREPARE` column-definition
    responses
- `crates/axiomdb-network/src/mysql/result.rs`
  - parameterize row/metadata serialization by result collation
  - return `Result<PacketSeq, DbError>` instead of silently assuming UTF-8
- `crates/axiomdb-network/tests/integration_protocol.rs`
  - add unit/protocol tests for handshake state, charset registry, result
    metadata collation ids, and prepared string param decoding
- `tools/wire-test.py`
  - add a live latin1 client regression and keep existing utf8mb4 regressions

## Reviewed first

These files were reviewed before writing this plan:

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

Research reviewed before writing this plan:

- `research/mariadb-server/sql/share/charsets/Index.xml`
- `research/mariadb-server/strings/ctype-utf8.c`
- `research/mariadb-server/mysql-test/main/ctype_collate.test`
- `research/mariadb-server/mysql-test/main/ctype_collate.result`
- `research/mariadb-server/sql/share/errmsg-utf8.txt`
- `research/postgres/src/backend/commands/variable.c`
- `research/postgres/src/backend/utils/mb/mbutils.c`
- `research/oceanbase/src/sql/session/ob_basic_session_info.h`
- `research/sqlite/src/insert.c`

## Research synthesis

### AxiomDB-first constraints

- `packets.rs` already extracts the handshake `character_set` byte but decodes
  `username` and `database` as UTF-8 too early
- `handler.rs` currently has three hardcoded UTF-8 decode sites:
  - `COM_INIT_DB`
  - `COM_QUERY`
  - `COM_STMT_PREPARE`
- `prepared.rs` currently has one lossy UTF-8 decode site for all string-like
  binary parameters
- `result.rs` currently hardcodes collation id `255` in both row metadata and
  prepare metadata
- `session.rs` currently cannot represent different values for:
  - `character_set_client`
  - `character_set_connection`
  - `character_set_results`
  - `collation_connection`

### What we borrow

- MariaDB/MySQL compatibility surface:
  - `research/mariadb-server/sql/share/charsets/Index.xml`
    - `latin1` is documented as `cp1252 West European`
    - verified ids: `latin1_swedish_ci = 8`, `latin1_bin = 47`,
      `utf8mb3_general_ci = 33`, `utf8mb3_bin = 83`, `binary = 63`
  - `research/mariadb-server/strings/ctype-utf8.c`
    - verified ids: `utf8mb4_general_ci = 45`, `utf8mb4_bin = 46`
  - `research/mariadb-server/mysql-test/main/ctype_collate.test`
  - `research/mariadb-server/mysql-test/main/ctype_collate.result`
    - `SET NAMES` / `SET CHARACTER SET` surface and charset/collation pair
      validation
  - `research/mariadb-server/sql/share/errmsg-utf8.txt`
    - `ER_UNKNOWN_CHARACTER_SET`
- PostgreSQL transport-boundary technique:
  - `research/postgres/src/backend/commands/variable.c`
    - validate/canonicalize requested encoding before applying it
  - `research/postgres/src/backend/utils/mb/mbutils.c`
    - conversion lives at the client/server edge, with a fast path when no
      conversion is needed but input still must be validated
- OceanBase session-state shape:
  - `research/oceanbase/src/sql/session/ob_basic_session_info.h`
    - separate fields for client, connection, results, and collation session
      state
- SQLite discipline:
  - `research/sqlite/src/insert.c`
    - keep conversion/validation at the layer boundary rather than polluting
      generic scalar containers

### What we reject

- copying MariaDB's full charset catalog into AxiomDB
- adding charset logic to `axiomdb-types`, the parser, or the executor
- any lossy replacement policy (`from_utf8_lossy`, replacement characters,
  best-effort encode)
- stringly-typed session charset state as the source of truth

### How AxiomDB adapts it

- a small static registry in `axiomdb-network`
- UTF-8 stays the internal engine representation
- conversion happens only at the MySQL boundary
- session state stores typed charset/collation pointers, while `@@vars` are
  derived from that typed state

## Algorithm / Data structure

### 1. Introduce a transport charset registry

Create `crates/axiomdb-network/src/mysql/charset.rs` with a small static
registry:

```rust
pub struct CharsetDef {
    pub canonical_name: &'static str,
    pub accepted_names: &'static [&'static str],
    pub default_collation: &'static CollationDef,
}

pub struct CollationDef {
    pub id: u16,
    pub name: &'static str,
    pub charset: &'static CharsetDef,
    pub is_binary: bool,
}
```

Static entries to define now:

- `utf8mb4_0900_ai_ci` (`255`) — server default
- `utf8mb4_general_ci` (`45`)
- `utf8mb4_bin` (`46`)
- `utf8mb3_general_ci` (`33`)
- `utf8mb3_bin` (`83`)
- `latin1_swedish_ci` (`8`)
- `latin1_bin` (`47`)
- `binary` (`63`) — metadata only

Charset helper API to expose:

```rust
pub const DEFAULT_SERVER_COLLATION: &CollationDef;
pub const BINARY_COLLATION: &CollationDef;

pub fn lookup_charset(name: &str) -> Option<&'static CharsetDef>;
pub fn lookup_collation(name: &str) -> Option<&'static CollationDef>;
pub fn lookup_collation_by_id(id: u16) -> Option<&'static CollationDef>;

pub fn decode_text(
    charset: &'static CharsetDef,
    bytes: &[u8],
) -> Result<std::borrow::Cow<'_, str>, DbError>;

pub fn encode_text(
    charset: &'static CharsetDef,
    text: &str,
) -> Result<std::borrow::Cow<'_, [u8]>, DbError>;
```

Chosen conversion behavior:

- `utf8mb4`
  - decode with `std::str::from_utf8`
  - encode is a borrow of the original UTF-8 bytes
- `utf8mb3`
  - decode with UTF-8 validation plus an explicit scan rejecting any scalar
    value above `U+FFFF`
  - encode rejects any scalar value above `U+FFFF`
- `latin1`
  - use `encoding_rs::WINDOWS_1252`
  - decode accepts every byte
  - encode rejects unrepresentable Unicode scalars if `encoding_rs` reports
    replacement would be needed

This registry is the single source of truth for:

- accepted session names
- handshake collation ids
- output metadata collation ids
- transport encode/decode policy

### 2. Refactor ConnectionState to typed charset state

Replace the current one-string model with:

```rust
pub struct ConnectionState {
    pub current_database: String,
    pub autocommit: bool,
    client_charset: &'static CharsetDef,
    connection_collation: &'static CollationDef,
    results_collation: &'static CollationDef,
    pub variables: HashMap<String, String>,
    ...
}
```

Why this exact shape:

- `character_set_client` is a charset, not a collation
- `character_set_connection` is derivable from `connection_collation.charset`
- `character_set_results` is derivable from `results_collation.charset`
- `collation_connection` needs a real collation object
- the wire layer also needs a concrete result collation id for column
  definitions, so a typed `results_collation` avoids re-deriving or guessing
  later

New `ConnectionState` helpers to add:

```rust
impl ConnectionState {
    pub fn new() -> Self; // default = utf8mb4_0900_ai_ci
    pub fn from_handshake_collation_id(id: u8) -> Result<Self, DbError>;

    pub fn character_set_client_name(&self) -> &'static str;
    pub fn character_set_connection_name(&self) -> &'static str;
    pub fn character_set_results_name(&self) -> &'static str;
    pub fn collation_connection_name(&self) -> &'static str;

    pub fn client_charset(&self) -> &'static CharsetDef;
    pub fn results_collation(&self) -> &'static CollationDef;

    pub fn decode_client_text(&self, bytes: &[u8]) -> Result<String, DbError>;
    pub fn decode_identifier_text(&self, bytes: &[u8]) -> Result<String, DbError>;
    pub fn encode_result_text(&self, text: &str) -> Result<Vec<u8>, DbError>;
}
```

`from_handshake_collation_id(id)` decision:

- supported id → initialize `client_charset`, `connection_collation`, and
  `results_collation` from that collation
- unsupported id → return `DbError::InvalidValue { reason: ... }`

`handler.rs` will not route that error through `dberror_to_mysql()`. It will
build the handshake ERR packet directly with code `1115` / SQLSTATE `42000`
and close. This keeps protocol-only handshake semantics out of the generic SQL
error mapper.

### 3. Keep the handshake parser byte-level

Change `packets.rs` so that handshake parsing remains low-level and does **not**
decode `username` or `database` text inside the packet parser.

Chosen structure:

```rust
pub struct HandshakeResponseRaw {
    pub capability_flags: u32,
    pub character_set: u8,
    pub username: Vec<u8>,
    pub auth_response: Vec<u8>,
    pub database: Option<Vec<u8>>,
    pub auth_plugin_name: Option<String>,
}

pub fn parse_handshake_response(payload: &[u8]) -> Option<HandshakeResponseRaw>;
```

Chosen reason:

- the packet parser can stay protocol-focused
- `handler.rs` already has the session context needed to decode handshake text
  correctly
- this avoids coupling `packets.rs` to the charset registry

Concrete handler sequence:

```text
payload = read handshake response
raw = parse_handshake_response(payload) or malformed ERR
conn_state = ConnectionState::from_handshake_collation_id(raw.character_set)
username = conn_state.decode_identifier_text(&raw.username) or malformed ERR
database = raw.database.map(decode_identifier_text).transpose() or malformed ERR
authenticate using decoded username
```

Malformed handshake decode for username/database stays mapped to the existing
malformed-handshake response, not to a SQL parser error.

### 4. Make every inbound text boundary use session charset helpers

Exact handler call-site changes:

- `COM_INIT_DB`
  - replace `String::from_utf8_lossy(body)` with
    `conn_state.decode_identifier_text(body)?`
- `COM_QUERY`
  - replace `std::str::from_utf8(body)` with
    `conn_state.decode_client_text(body)?`
- `COM_STMT_PREPARE`
  - replace `std::str::from_utf8(body)` with
    `conn_state.decode_client_text(body)?`

Prepared binary params:

Change:

```rust
pub fn parse_execute_packet(
    payload: &[u8],
    stmt: &mut PreparedStatement,
) -> Result<ExecutePacket, DbError>
```

to:

```rust
pub fn parse_execute_packet(
    payload: &[u8],
    stmt: &mut PreparedStatement,
    client_charset: &'static CharsetDef,
) -> Result<ExecutePacket, DbError>
```

and thread `conn_state.client_charset()` from `handler.rs`.

Inside `prepared.rs`:

- `read_lenenc_str(...)` becomes charset-aware
- string-like type codes use charset-aware decode
- `Bytes` are still returned as `Value::Text(...)` today for string-like MySQL
  protocol values because that is the current prepared-parameter contract in
  AxiomDB; `5.2a` changes the decode policy, not the parameter AST/value model

### 5. Make result serialization charset-aware and non-lossy

Current `result.rs` silently assumes UTF-8 and returns `PacketSeq`. That is not
good enough once `character_set_results` can be `latin1` or `utf8mb3`.

Chosen signature changes:

```rust
pub fn serialize_query_result(
    result: QueryResult,
    seq_start: u8,
    results_collation: &'static CollationDef,
) -> Result<PacketSeq, DbError>;

pub fn serialize_query_result_multi_warn(
    result: QueryResult,
    seq_start: u8,
    more_results: bool,
    warning_count: u16,
    results_collation: &'static CollationDef,
) -> Result<PacketSeq, DbError>;

pub fn serialize_query_result_binary(
    result: QueryResult,
    seq_start: u8,
    results_collation: &'static CollationDef,
) -> Result<PacketSeq, DbError>;
```

Chosen reason:

- outbound encode can fail for `latin1` / `utf8mb3`
- silently replacing data would be wrong
- the handler already has an ERR path for `DbError`

Exact metadata rule:

- `Text`, `Decimal`, and `Uuid` column definitions use
  `results_collation.id`
- `Bytes` uses `BINARY_COLLATION.id` (`63`)
- numeric/date/timestamp/bool metadata also uses `BINARY_COLLATION.id` (`63`)

Exact row rule:

- text-like cells are encoded via `encode_text(results_collation.charset, ...)`
- bytes remain raw bytes
- numeric/date/timestamp/bool remain existing binary/text serialization

Exact prepare metadata rule:

Change `build_prepare_response(...)` in `prepared.rs` to accept
`results_collation` and use it for:

- parameter stub column definitions (`VAR_STRING`)
- result column definitions emitted by `COM_STMT_PREPARE`

### 6. Keep `apply_set()` on the existing Result path, but resolve semantics now

Keep:

```rust
pub fn apply_set(&mut self, sql: &str) -> Result<bool, DbError>
```

No new wrapper enum is needed.

Concrete chosen semantics:

- `SET NAMES charset`
  - look up `charset`
  - set:
    - `client_charset = charset`
    - `connection_collation = charset.default_collation`
    - `results_collation = charset.default_collation`
- `SET NAMES charset COLLATE collation`
  - look up both
  - require `collation.charset == charset`
  - set:
    - `client_charset = charset`
    - `connection_collation = collation`
    - `results_collation = collation`
- `SET character_set_client = charset`
  - set only `client_charset`
- `SET character_set_connection = charset`
  - set only `connection_collation = charset.default_collation`
- `SET character_set_results = charset`
  - set only `results_collation = charset.default_collation`
- `SET collation_connection = collation`
  - set only `connection_collation = collation`
- unsupported charset/collation names
  - return `DbError::InvalidValue`
- incompatible charset/collation pair
  - return `DbError::InvalidValue`

Chosen explicit non-goal:

- do **not** add `SET CHARACTER SET` in `5.2a`
- do **not** add a user-visible `collation_results`

### 7. Handler and helper call sites that must change

These are the exact places that must be wired, so the implementation does not
have to invent the integration later:

- `handler.rs`
  - greeting: `build_server_greeting(...)` uses
    `charset::DEFAULT_SERVER_COLLATION.id`
  - post-handshake: initialize `conn_state` from handshake collation id
  - `COM_INIT_DB`: decode with `decode_identifier_text`
  - `COM_QUERY`: decode with `decode_client_text`
  - `COM_STMT_PREPARE`: decode with `decode_client_text`
  - `COM_STMT_EXECUTE`: pass `conn_state.client_charset()` into
    `parse_execute_packet(...)`
  - every `serialize_query_result*` call passes
    `conn_state.results_collation()`
- `intercept_special_query(...)`
  - keep return type `InterceptResult`
  - every helper inside it that builds a rowset must call the new serializer
    with `conn_state.results_collation()`
- `show_variables_result(...)`
  - stop fabricating charset vars from `character_set_client` alone
  - derive values from typed session state
- `single_text_row(...)` and `single_null_row(...)`
  - accept `results_collation` so their metadata is encoded consistently

## Implementation phases

1. Add `charset.rs` with the supported registry, alias normalization, cp1252
   latin1 handling, utf8mb3 validation, and direct unit tests.
2. Refactor `ConnectionState` to typed charset/collation state with getters and
   coherent `SET` semantics.
3. Change handshake parsing to raw username/database bytes and initialize
   session charset state from the handshake collation id in `handler.rs`.
4. Replace all inbound hardcoded UTF-8 decode sites in `handler.rs` with
   session-aware decode helpers.
5. Make `prepared.rs` string parameter decoding charset-aware.
6. Make `result.rs` and `prepared.rs` metadata serialization
   result-collation-aware and non-lossy.
7. Update protocol/unit tests and wire smoke coverage for default utf8mb4 plus
   latin1 regression.

## Tests to write

- unit:
  - `latin1` decode byte `0x80` → `€`
  - `latin1` encode `€` → byte `0x80`
  - `latin1` encode of an unrepresentable scalar returns `Err`
  - `utf8mb3` decode rejects 4-byte UTF-8
  - `utf8` alias normalizes to `utf8mb3`
  - `SET NAMES latin1 COLLATE latin1_bin` updates typed state coherently
  - invalid `SET NAMES latin1 COLLATE utf8mb3_bin` preserves previous state
  - `SET character_set_results = latin1` changes only results state
  - `COM_STMT_EXECUTE` string param bytes are decoded with latin1/cp1252
  - text column defs use selected result collation id
  - bytes/numeric metadata use binary collation id `63`
- integration:
  - handshake response with `character_set = 8` initializes latin1 session vars
  - unsupported handshake collation id is rejected before auth success
  - `COM_RESET_CONNECTION` resets charset session vars to utf8mb4 defaults
  - intercept rowsets (`SHOW VARIABLES`, `SELECT @@...`) serialize with the
    selected result collation
- wire:
  - default client still connects and passes the previous utf8mb4 regressions
  - a latin1 client can connect, `SET NAMES latin1`, insert/select `café`
  - a prepared statement with a latin1 string parameter round-trips
  - a latin1 client selecting an emoji gets ERR instead of replacement bytes
- bench:
  - none for `5.2a`; this is correctness-first transport work

## Anti-patterns to avoid

- do not keep `character_set_client` as the only source of truth and derive
  every other charset variable by copy
- do not leave `String::from_utf8_lossy(...)` anywhere on the MySQL text path
- do not silently replace unrepresentable outbound characters
- do not put charset conversion logic into `axiomdb-types` or SQL executor code
- do not hardcode more numeric collation ids in multiple files after adding the
  registry
- do not let `COM_RESET_CONNECTION` restore the original handshake choice; it
  must restore server defaults

## Risks

- Risk: `result.rs` signature changes ripple across many helpers.
  - Mitigation: change all serializer entrypoints in one pass and keep
    `handler.rs` as the single wiring point.
- Risk: output-encoding failures could be missed in intercept paths.
  - Mitigation: every helper that returns a rowset must route through the same
    result serializer and return `Result`.
- Risk: `latin1` implemented as ISO-8859-1 would break MySQL compatibility for
  bytes like `0x80`.
  - Mitigation: explicitly use cp1252-compatible `encoding_rs::WINDOWS_1252`
    and add the `€` regression test.
- Risk: utf8mb3 behavior silently accepts 4-byte UTF-8 because the Rust UTF-8
  decoder accepts it.
  - Mitigation: add an explicit post-decode scalar scan and dedicated tests.
- Risk: handshake text fields stay UTF-8-only if the parser continues to decode
  them too early.
  - Mitigation: keep handshake parser byte-level and decode in `handler.rs`
    after session charset selection.
