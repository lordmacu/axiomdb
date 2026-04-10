# Fase 11 - Robustness and indexes

## 11.4 Native JSON - cerrada 2026-04-10

La subfase 11.4 entrega un tipo SQL `JSON` nativo visible en DDL, catalogo,
row codec, coercion, evaluador SQL, API embedded y MySQL wire. El alcance
cerrado es intencionalmente text-backed: los valores se almacenan como UTF-8
JSON validado y normalizado a NFC con el mismo payload u24 que `TEXT`, pero se
mantienen como `Value::Json` / `DataType::Json` / `ColumnType::Json` para no
perder la semantica de tipo.

Superficie SQL implementada:

```sql
CREATE TABLE docs (id INT, data JSON);
INSERT INTO docs VALUES (1, '{"name":"Alice","age":30}');

SELECT JSON_EXTRACT(data, '$.name') FROM docs;
SELECT data->>'name' FROM docs WHERE data->>'name' = 'Alice';
SELECT JSON_SET(data, '$.active', TRUE) FROM docs;
SELECT JSON_REMOVE(data, '$.age') FROM docs;
SELECT JSON_KEYS(data), JSON_VALID(data), JSON_TYPE(data) FROM docs;
```

`JSON_EXTRACT` convierte escalares JSON a valores SQL (`TEXT`, `INT`,
`BIGINT`, `REAL`, `BOOL`, `NULL`) y conserva objetos/arreglos como
`Value::Json`. `JSON_SET`, `JSON_REMOVE`, `JSON_KEYS`, `JSON_VALID` y
`JSON_TYPE` cubren rutas simples. El operador PostgreSQL-style `->>` se baja en
parser a `JSON_EXTRACT(expr, '$.field')` para reutilizar el mismo evaluador que
la sintaxis MySQL-style.

## Diferido

La entrada original de progreso mezclaba Native JSON con el roadmap completo de
JSONB/GIN. Queda diferido explicitamente: layout binario JSONB, indexacion GIN
automatica, SQL:2016 JSONPath completo, operador `->`, `JSON_MERGE_PATCH`,
operadores de contencion y actualizaciones JSONB sin reescribir el documento.
Eso requiere un subphase dedicado de formato/indexacion y no se debe marcar como
parte del cierre 11.4 text-backed.

## Validacion

- `cargo test -p axiomdb-types --test integration_row_codec` - paso, 35 tests.
- `cargo test -p axiomdb-sql --test integration_json` - paso, 6 tests.
- `cargo test -p axiomdb-catalog` - paso.
- `cargo test --workspace` - paso despues de liberar `target/` por un fallo ambiental de disco lleno.
- `cargo clippy --workspace -- -D warnings` - paso.
- `cargo fmt --check` - paso.
- `cargo build -p axiomdb-server` - paso.
- `tools/wire-test.py` - paso, 341/341 assertions.

## Benchmark

| Benchmark | AxiomDB | MySQL (aprox) | PostgreSQL (aprox) | Target | Max aceptable | Veredicto |
|---|---:|---:|---:|---:|---:|---|
| json_extract/10K | 28.7 ms / 348,652 rows/s | No corrido localmente | No corrido localmente | escenario agregado + smoke local | sin limite formal 11.4 | ✅ |

Comando usado:

```bash
PYTHONDONTWRITEBYTECODE=1 python3 benches/comparison/local_bench.py --scenario json_extract --rows 10000 --engines axiomdb --table
```
