# Plan: 11.4 — Native JSON Type

## Files to create/modify

- `crates/axiomdb-types/src/value.rs` — add `Value::Json(String)` and display
  behavior.
- `crates/axiomdb-types/src/types.rs` — add `DataType::Json` and SQL name.
- `crates/axiomdb-types/src/codec.rs` — encode/decode JSON with the text wire
  shape while preserving `DataType::Json`.
- `crates/axiomdb-types/src/coerce_api.rs` — validate text-to-JSON coercion.
- `crates/axiomdb-types/src/field_patch.rs` — treat JSON as variable-width data
  where field patching needs to skip payloads.
- `crates/axiomdb-catalog/src/schema_database.rs` — reserve catalog
  `ColumnType::Json = 9`.
- `crates/axiomdb-catalog/src/schema.rs` — update schema catalog tests for the
  JSON discriminant.
- `crates/axiomdb-sql/src/lexer.rs` — add `JSON` type token and `->>` operator
  token.
- `crates/axiomdb-sql/src/parser/ddl.rs` — parse JSON column definitions.
- `crates/axiomdb-sql/src/parser/expr.rs` — parse `json_expr->>'key'` as a JSON
  extraction binary operation or function-call sugar.
- `crates/axiomdb-sql/src/expr.rs` — represent the JSON extraction operator if
  the parser does not lower it directly to `JSON_EXTRACT`.
- `crates/axiomdb-sql/src/eval/functions/json.rs` — implement JSON scalar
  functions.
- `crates/axiomdb-sql/src/eval/functions/mod.rs` — route JSON function names.
- `crates/axiomdb-sql/src/eval/ops.rs` — evaluate `->>` if represented as a
  binary operator.
- `crates/axiomdb-sql/src/eval/core.rs`, `crates/axiomdb-sql/src/eval/batch.rs`,
  `crates/axiomdb-sql/src/executor/*`, and `crates/axiomdb-sql/src/table.rs` —
  propagate JSON type/value matching wherever text-like values are hashed,
  compared, batched, displayed, or inferred.
- `crates/axiomdb-network/src/mysql/result.rs` and
  `crates/axiomdb-network/src/mysql/prepared.rs` — serialize JSON on the MySQL
  wire as VAR_STRING payloads.
- `crates/axiomdb-embedded/src/lib.rs` — expose JSON values through embedded
  result conversion.
- `crates/axiomdb-types/tests/integration_row_codec.rs` — add JSON codec tests.
- `crates/axiomdb-sql/tests/integration_json.rs` — add JSON DDL, DML, function,
  operator, and NULL tests.
- `benches/comparison/local_bench.py` — add `json_extract` comparison scenario.
- `tools/wire-test.py` — add JSON wire smoke assertions if JSON is observable
  through the MySQL wire protocol in this subphase.
- `docs-site/src/user-guide/sql-reference/data-types.md` — document `JSON`.
- `docs-site/src/user-guide/sql-reference/expressions.md` — document JSON
  functions and `->>`.
- `docs-site/src/user-guide/errors.md` — document invalid JSON insert error.
- `docs-site/src/internals/row-codec.md` — document JSON row encoding.
- `docs-site/src/internals/sql-parser.md` — document JSON parser/evaluator
  support.
- `docs-site/src/development/roadmap.md` — update the Phase 11 JSON status when
  the subphase is closed.
- `docs/progreso.md`, `docs/fase-11.md`, and `memory/*.md` — closeout updates
  after implementation and review pass.

## Algorithm / Data structure

JSON is represented as:

```rust
enum Value {
    Json(String),
}

enum DataType {
    Json,
}

enum ColumnType {
    Json = 9,
}
```

Codec pseudocode:

```text
encode Value::Json(s):
    normalized = NFC(s)
    validate normalized is UTF-8 string already held by Rust String
    write u24(normalized.len)
    write normalized bytes

decode DataType::Json:
    len = read_u24()
    if len is TOAST sentinel:
        return JSON placeholder for detoast path
    bytes = read len
    s = utf8(bytes) or error
    return Value::Json(s)
```

Coercion pseudocode:

```text
coerce(Text(s), Json):
    parse serde_json::Value from s
    if ok: return Json(s)
    else: InvalidValue("invalid JSON: ...")

coerce(Json(s), Text):
    return Text(s)
```

Function pseudocode:

```text
JSON_EXTRACT(json, path):
    if json is SQL NULL: return SQL NULL
    parsed = serde_json::from_str(json)
    node = follow simple path
    return node converted to SQL scalar or Json(document)

JSON_SET(json, path, value):
    if json is SQL NULL: return SQL NULL
    parsed = serde_json::from_str(json)
    set object key at simple path to sql_to_json(value)
    return Value::Json(parsed.to_string())

JSON_REMOVE(json, path):
    if json is SQL NULL: return SQL NULL
    parsed = serde_json::from_str(json)
    remove object key at simple path
    return Value::Json(parsed.to_string())
```

Operator plan:

```text
parse json_expr ->> 'field':
    lower to JSON_EXTRACT(json_expr, '$.field')

parse json_expr ->> path_expr:
    if path_expr is a string starting with '$', use it as-is
    otherwise prefix '$.' for compatibility with PostgreSQL field extraction
```

Lowering to a function call is preferred over adding a new `BinaryOp` because it
reuses existing function dispatch and avoids updating every binary-op formatter.

## Implementation phases

1. Normalize the current partial JSON type changes so all touched crates compile
   without relying on placeholder behavior that bypasses validation.
2. Add parser support for `->>` and lower it to `JSON_EXTRACT`.
3. Fix JSON function NULL semantics and path validation so invalid paths are
   explicit errors, not silent empty-string behavior.
4. Add row codec and coercion tests for valid JSON, invalid JSON, and masked
   decode behavior.
5. Add SQL integration tests for JSON DDL, inserts, invalid inserts, extraction,
   updates through JSON functions, `->>` in `SELECT` and `WHERE`, and NULL cases.
6. Add the `json_extract` benchmark scenario and run the targeted benchmark for
   this subphase.
7. Update docs-site user and internals pages with examples, errors, storage
   layout, and a design callout explaining the text-backed Phase 11.4 choice
   versus PostgreSQL JSONB.
8. Run targeted checks for touched crates, then the required closing gates before
   marking 11.4 done.

## Tests to write

- unit: row codec round-trip for `Value::Json`, including `decode_row_masked`.
- unit: coercion accepts valid JSON text and rejects invalid JSON text.
- unit: parser parses `JSON` as a data type and parses or lowers `->>`.
- integration: `CREATE TABLE t (id INT, data JSON)` plus valid/invalid inserts.
- integration: `JSON_EXTRACT`, `JSON_SET`, `JSON_REMOVE`, `JSON_KEYS`,
  `JSON_VALID`, and `JSON_TYPE` return expected values.
- integration: `data->>'name'` works in projection and `WHERE`.
- integration: SQL NULL passed to JSON functions follows the spec.
- integration: MySQL wire smoke test returns JSON values as text payloads.
- bench: `local_bench.py` `json_extract` scenario against MariaDB comparison.

## Anti-patterns to avoid

- DO NOT mark 11.4 complete if binary JSONB or automatic GIN remain implied by
  the progress entry without an explicit deferred item.
- DO NOT silently accept invalid JSON into `JSON` columns.
- DO NOT decode JSON columns as `Value::Text`; the wire bytes can match text, but
  the in-memory type must preserve `Value::Json`.
- DO NOT implement full SQL:2016 JSONPath accidentally through ad-hoc parsing;
  only simple object paths are in scope.
- DO NOT use `unwrap()` in production JSON path mutation code.
- DO NOT add a separate wire type that MySQL clients cannot consume; expose JSON
  as VAR_STRING until a dedicated protocol mapping exists.

## Risks

- Scope mismatch with `docs/progreso.md` broader JSONB/GIN text -> mitigation:
  keep the spec deferred section and do not claim the broader work is done.
- TOAST placeholder decoding for JSON can leak internal sentinel strings if the
  detoast scan path misses JSON -> mitigation: add a large JSON integration test
  if feasible after basic JSON tests pass.
- `->>` tokenization conflicts with existing `-` and `>>` tokens -> mitigation:
  add the longer `->>` token before shorter operator tokens and test the lexer.
- JSON function NULL behavior may diverge from MySQL/PostgreSQL -> mitigation:
  encode the Phase 11.4 behavior in tests and docs.
- Text-backed JSON has slower repeated extraction than PostgreSQL JSONB ->
  mitigation: benchmark and document this as an intentional Phase 11.4 trade-off,
  with binary JSONB deferred.
