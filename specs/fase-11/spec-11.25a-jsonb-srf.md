# Spec: 11.25a — JSONB set-returning functions (PG parity)

## What to build

Five PostgreSQL-compatible set-returning functions over JSONB,
usable in `FROM` position, join right sides, and APPLY / LATERAL
forms:

| Function | Output columns | Input requirement |
|---|---|---|
| `jsonb_each(doc)` | `(key TEXT, value JSONB)` | object |
| `jsonb_each_text(doc)` | `(key TEXT, value TEXT)` | object |
| `jsonb_object_keys(doc)` | `(key TEXT)` | object |
| `jsonb_array_elements(doc)` | `(value JSONB)` | array |
| `jsonb_array_elements_text(doc)` | `(value TEXT)` | array |

Non-matching input type → clear error (PG parity: `cannot call
jsonb_each on a non-object`). NULL doc → zero rows.

## Grammar

```
from_item := ... | jsonb_srf_call [ alias ]

jsonb_srf_call := ('jsonb_each' | 'jsonb_each_text' | 'jsonb_object_keys' |
                   'jsonb_array_elements' | 'jsonb_array_elements_text')
                  '(' expr ')'
```

Dispatch in `parse_from_item`: when a bare identifier matches one of
the five names and is followed by `(`, consume as SRF. Matches the
existing `JSON_TABLE` dispatch pattern.

## AST

New:

```rust
pub enum JsonbSrfKind {
    Each,            // (key TEXT, value JSONB)
    EachText,        // (key TEXT, value TEXT)
    ObjectKeys,      // (key TEXT)
    ArrayElements,   // (value JSONB)
    ArrayElementsText,// (value TEXT)
}

pub struct JsonbSrf {
    pub kind: JsonbSrfKind,
    pub doc: Expr,
    pub alias: Option<String>,
}
```

Extend `FromClause`:

```rust
enum FromClause {
    Table(TableRef),
    Subquery { ... },
    JsonTable(Box<JsonTable>),
    JsonbSrf(Box<JsonbSrf>),  // new
}
```

## Executor

Mirror `FromClause::JsonTable` handling at three sites:

1. `execute_select_core.rs` — new `execute_select_jsonb_srf_source` for
   first-FROM SRF (no joins) + route to joins pipeline for multi-table.
2. `select_joins_ctx.rs` — new join-side arm that materializes per-kind.
3. `dml_join.rs` — same arm for UPDATE/DELETE joins.

Correlation: follows the same `jsontable_is_correlated`-style predicate
on the `doc` expression. Correlated SRF (doc references outer columns)
re-materializes per outer row using the per-outer-row join helper
pattern established by 11.20d3 / 11.20d4.

Row materialization (non-correlated, evaluated once):

```rust
fn materialize_jsonb_srf(kind, doc, outer_row) -> Vec<Row> {
    let val = eval(doc, outer_row)?;
    let sj = match val { Null => return vec![], text/json/jsonb => to_serde(val)? };
    match kind {
        Each | EachText | ObjectKeys => {
            let obj = sj.as_object().ok_or(TypeMismatch)?;
            obj.iter().map(|(k, v)| row_for_kind(kind, k, v)).collect()
        }
        ArrayElements | ArrayElementsText => {
            let arr = sj.as_array().ok_or(TypeMismatch)?;
            arr.iter().map(|v| row_for_kind_array(kind, v)).collect()
        }
    }
}
```

## Acceptance criteria

- [ ] `SELECT * FROM jsonb_each('{"a":1,"b":2}')` returns 2 rows
      `(a, 1)` and `(b, 2)`.
- [ ] `SELECT * FROM jsonb_each_text('{"a":1}')` returns
      `(a, '1')` — value as TEXT.
- [ ] `SELECT * FROM jsonb_object_keys('{"a":1,"b":2}')` returns
      2 rows with single column.
- [ ] `SELECT * FROM jsonb_array_elements('[1,2,3]')` returns
      3 rows.
- [ ] `SELECT * FROM jsonb_array_elements_text('["x","y"]')`
      returns `'x'`, `'y'` (unquoted).
- [ ] `jsonb_each` on non-object → error mentioning the function
      name.
- [ ] `jsonb_array_elements` on non-array → error.
- [ ] NULL doc → zero rows.
- [ ] JOIN with SRF right side works non-correlated and correlated
      (LATERAL).
- [ ] CROSS APPLY / OUTER APPLY with SRF works.
- [ ] UPDATE / DELETE with SRF right side works.
- [ ] 10–14 integration tests.
- [ ] 2 wire smoke assertions.

## Out of scope

- `json_*` (non-b) variants — MySQL does not have them as SRFs; add
  only if demanded. AxiomDB's `jsonb_*` family already accepts both
  JSONB and JSON-text input via `doc_to_serde`.
- `jsonb_to_record` / `jsonb_to_recordset` — require a defined output
  record type, separate feature (11.25c construction helpers or
  later).
- `jsonb_populate_record` — same.

## Dependencies

- Phase 11.16 (JSONB binary + `JsonbRef`)
- Phase 11.20a–d4 (join pipeline accepting any `FromClause`
  variant, correlation machinery)
