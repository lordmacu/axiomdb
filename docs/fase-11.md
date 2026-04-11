# Fase 11 - Robustness and indexes

## 11.2d BLOB reference tracking - cerrada 2026-04-10

La subfase 11.2d agrega un formato versionado para cadenas TOAST/BLOB
refcounted en `crates/axiomdb-storage/src/clustered_overflow.rs`. La
investigacion se hizo en `research/`:

- PostgreSQL TOAST usa chunks por OID de valor, sin contador por cadena.
- SQLite overflow usa una lista enlazada de paginas, sin sharing/refcount.
- InnoDB/MariaDB guarda metadata de propiedad en punteros BLOB externos, util
  para MVCC, pero no suficiente para deduplicacion N-a-1.

La opcion elegida fue mantener las cadenas clustered overflow existentes para
filas clustered, y agregar un formato `ABOB` solo para cadenas TOAST/BLOB:

```text
[magic="ABOB"][version=1][flags][reserved][next_page:u64][part_len:u32][refcount:u64][payload]
```

Solo la primera pagina posee el contador. Las paginas de continuacion cargan el
mismo magic/version y `part_len`, pero `refcount = 0`. `read_blob_chain()`
detecta el formato nuevo y usa `part_len`; si el magic no existe, cae al lector
legacy `read_chain()` con longitud esperada. `free_blob()` decrementa el
contador y libera la cadena solo cuando llega a cero. `incref_blob()` queda
implementado ahora como primitiva para Phase 14.9 content-addressed dedup.

Integracion:

- `toast_row_if_needed()` escribe TOAST con `write_refcounted_chain()`.
- `detoast_row()` resuelve placeholders `__toast__:page_id:compressed:raw_len`
  mediante `read_blob_chain()`.
- `free_toast_chains_in_encoded()` libera mediante `free_blob()`.
- El codec conserva `raw_len` en los placeholders de `Text`, `Json` y `Bytes`
  para que las cadenas legacy sigan leyendo con longitud exacta.

Estado de cierre: implementado, validado y marcado cerrado en
`docs/progreso.md`. Durante el cierre se limpiaron warnings mecanicos de tests
activados por `clippy` y se corrigio un bug real en `CacheShard::evict_if_needed()`:
si el shard excedia capacidad y todas las paginas candidatas estaban pinned, el
evictor podia ciclar indefinidamente. Ahora escanea el conjunto LRU una vez y
sale si no hay pagina evictable.

Validacion dirigida:

- `cargo test -p axiomdb-storage clustered_overflow --lib` - paso, 9 tests.
- `cargo test -p axiomdb-types --test integration_row_codec test_decode_row_masked_json_toast_pointer` - paso.
- `cargo test -p axiomdb-sql --test integration_table` - paso, 12 tests.
- `cargo clippy -p axiomdb-storage -p axiomdb-types -p axiomdb-sql --lib -- -D warnings` - paso.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.
- `cargo fmt --check` - paso.
- `cargo build -p axiomdb-server` - paso.
- `tools/wire-test.py` - paso, 344/344 assertions.

Gap descubierto durante wire smoke:

- `COM_QUERY` con literales `TEXT` largos (~7 KB+) puede desbordar el stack del
  worker tokio antes de llegar al almacenamiento. No pertenece al formato
  `ABOB` ni al detoast/refcount; se reprodujo tambien con payloads debajo del
  umbral TOAST. El smoke 11.2d usa `COM_STMT_SEND_LONG_DATA`, que es la ruta
  wire correcta para BLOBs grandes y valida la cadena TOAST/BLOB refcounted.

Benchmark:

| Benchmark | AxiomDB | MySQL (aprox) | PostgreSQL (aprox) | Target | Max aceptable | Veredicto |
|---|---:|---:|---:|---:|---:|---|
| overflow/refcounted_blob/write_12kb | 89.896 µs / 130.36 MiB/s | N/A | N/A | sin limite formal 11.2d | sin limite formal 11.2d | ✅ |
| overflow/refcounted_blob/read_128kb | 20.355 µs / 5.997 GiB/s | N/A | N/A | sin limite formal 11.2d | sin limite formal 11.2d | ✅ |
| overflow/refcounted_blob/incref_free_shared_128kb | 25.241 µs / 39.618 Kops/s | N/A | N/A | sin limite formal 11.2d | sin limite formal 11.2d | ✅ |

Comando usado:

```bash
cargo bench -p axiomdb-storage --bench storage overflow/refcounted_blob 2>&1 | tee /tmp/bench-fase-11-2d.txt
```

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
