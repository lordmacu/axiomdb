# Fase 11 - Robustness and indexes

## 11.18c JSONB path operators - cerrada 2026-04-22

La subfase 11.18c queda cerrada como follow-up de paridad PostgreSQL para los
operadores JSONB `#>`, `#>>` y `#-`, pero con el contrato real que ya existe en
el repo: AxiomDB acepta una ruta RHS como **array JSONB** (`CAST('["a",1]' AS
JSONB)`) en vez del `text[]` nativo de PostgreSQL. El cierre no mete arrays al
type-system; documenta y valida explicitamente esa divergencia.

Comportamiento cerrado:

- `doc #> path_jsonb` extrae un subtree/valor JSONB y devuelve `NULL` si la
  ruta no existe.
- `doc #>> path_jsonb` extrae el valor y lo renderiza como texto SQL, quitando
  comillas externas para strings JSON.
- `doc #- path_jsonb` elimina una clave/indice/rama anidada; una ruta ausente
  es no-op.
- `NULL` a izquierda o derecha propaga `NULL`.

Componentes relevantes:

- Lexer: `Token::JsonPathExtract`, `JsonPathExtractText`, `JsonPathDelete` en
  `crates/axiomdb-sql/src/lexer.rs`.
- Parser: precedencia/parseo infix en
  `crates/axiomdb-sql/src/parser/expr.rs`.
- Evaluador: `eval_jsonb_path_extract` / `eval_jsonb_path_delete` en
  `crates/axiomdb-sql/src/eval/ops.rs`.
- Cobertura: `crates/axiomdb-sql/tests/integration_jsonb_path_ops.rs`.

Punto importante del cierre:

- El stripping de comentarios `#` queda acotado a inicio de linea para no
  romper los operadores `#>`, `#>>`, `#-` ni los literales JSON que contienen
  `#`.
- La dependencia historica de `TEXT[]` se elimina de la documentacion de
  roadmap para esta subfase; el contrato aceptado es JSONB-array RHS.

Validacion de cierre:

- `cargo test -p axiomdb-sql --test integration_jsonb_path_ops` - paso, 10/10.
- `tools/wire-test.py` - paso.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.
- `cargo fmt --check` - paso.

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

## 11.16 Binary JSONB + JSONPath - cerrada 2026-04-10

La subfase 11.16 entrega el tipo `JSONB` con layout binario, el operador `->`,
funciones de contencion y un compilador JSONPath completo.

### Tipo y codec

- `Value::Jsonb(Arc<Vec<u8>>)` — datos en el formato binario, compartidos via `Arc`
- `DataType::Jsonb` / `ColumnType::Jsonb = 10` — disco estable
- `CAST('...' AS JSONB)` convierte texto JSON a binario; `TO_JSONB('...')` es alias
- Wire: se serializa como texto JSON sobre el protocolo MySQL (mismo payload que `TEXT`)

### Layout binario (PostgreSQL jsonb.c — JEntry + stride)

```text
Container (object u array):
  [0..3]     u32 header: bit31=1→array, bits30..0=count N
  [4..E*4+3] JEntry array: E = 2*N (object) o N (array)
               Object: key JEntries [0..N), value JEntries [N..2N)
  [E*4+4..]  Data section: keys ordenadas bytewise-length-first, luego valores

JEntry (u32):
  bit31:     HAS_OFF (0=longitud, 1=offset absoluto desde data start)
  bits30..28 tipo: 0b000=string, 0b001=numeric, 0b010=false,
                    0b011=true, 0b100=null, 0b101=container
  bits27..0  longitud o offset (max 256 MB)

Stride=32: cada 32a JEntry almacena offset absoluto → element_offset(i) O(1).
Key sort: bytewise-length-first → busqueda binaria con short-circuit.
Encoder: DFS iterativo con Vec<Frame>; limite 256 niveles.
```

### Operador `->` y `->>`

- `data->'key'` — `BinaryOp::JsonSub` — extrae sub-valor como `Jsonb` (container) o escalar
- `data->>'key'` — ya existia; sigue bajando a `JSON_EXTRACT` que retorna texto
- `data->'tags'->0` — cadena de operadores: primero extrae array, luego indexa

### Funciones nuevas

| Funcion | Descripcion |
|---|---|
| `JSON_MERGE_PATCH(a, b)` | RFC 7396 merge patch |
| `JSON_CONTAINS(doc, query)` | 1 si query es subconjunto de doc |
| `JSON_OVERLAPS(a, b)` | 1 si tienen algun elemento en comun |
| `JSON_ARRAY_LENGTH(arr)` | longitud del array (NULL si no es array) |
| `JSON_DEPTH(val)` | profundidad maxima de anidamiento |
| `JSON_PRETTY(val)` | texto formateado con indentacion |
| `TO_JSONB(text)` | convierte JSON text a `Value::Jsonb` |

### JSONPath (`crates/axiomdb-sql/src/eval/jsonpath.rs`)

Compilador + ejecutor con modos lax/strict:

| Expresion | Significado |
|---|---|
| `$` | raiz |
| `$.key` | campo de objeto |
| `$[0]` | elemento de array por indice |
| `$.*` | todos los valores de un objeto |
| `$[*]` | todos los elementos de un array |
| `$..key` | descenso recursivo (todos los `key` a cualquier profundidad) |
| `$[?(@.field > val)]` | filtro por predicado |

Funciones expuestas:
- `JSON_PATH_EXISTS(doc, path)` → `Bool`
- `JSON_PATH_QUERY(doc, path)` → `Json` (array de resultados)
- `JSON_PATH_QUERY_FIRST(doc, path)` → escalar o `Null`

### Actualizacion de funciones 11.4

Todas las funciones JSON existentes (`JSON_EXTRACT`, `JSON_SET`, `JSON_REMOVE`,
`JSON_KEYS`, `JSON_TYPE`, `JSON_VALID`) detectan `Value::Jsonb` en el primer
argumento y usan el codec binario en lugar de re-parsear texto.

### Validacion

- `cargo test -p axiomdb-sql --test integration_jsonb` — paso, 20 tests.
- `cargo test -p axiomdb-types` — paso.
- `cargo test --workspace` — paso.
- `cargo clippy --workspace -- -D warnings` — paso.
- `cargo fmt --check` — paso.
- `tools/wire-test.py` — paso, 350/350 assertions.

### Benchmark

| Benchmark | AxiomDB | MySQL (aprox) | PostgreSQL (aprox) | Target | Max aceptable | Veredicto |
|---|---:|---:|---:|---:|---:|---|
| jsonb/encode_small | sin referencia formal | N/A | N/A | sin limite 11.16 | sin limite 11.16 | ✅ |
| jsonb/decode_small | sin referencia formal | N/A | N/A | sin limite 11.16 | sin limite 11.16 | ✅ |
| jsonb/get_key | sin referencia formal | N/A | N/A | sin limite 11.16 | sin limite 11.16 | ✅ |

---

## 11.20a — `JSON_TABLE` flat row source (2026-04-13)

### Resumen

`JSON_TABLE(doc, '$.row_path' COLUMNS (...))` ahora funciona como fuente de
filas en `FROM`. Cubre: `name TYPE PATH '$.x'`, `name FOR ORDINALITY`,
`name TYPE EXISTS PATH '$.x'`, con clausulas `ON EMPTY` / `ON ERROR`
(`NULL` / `ERROR` / `DEFAULT expr`) y `TRUE|FALSE|UNKNOWN|ERROR ON ERROR`
en columnas EXISTS. No incluye `NESTED PATH` (11.20b/c) ni WRAPPER/QUOTES/
PASSING sobre la row-path (11.20d).

### Componentes

- AST: `FromClause::JsonTable(Box<JsonTable>)`, `JsonTableColumn::{Regular,
  Ordinality, Exists}` en `crates/axiomdb-sql/src/ast.rs`.
- Parser: `crates/axiomdb-sql/src/parser/json_table.rs` + dispatch
  peek-`(` en `parse_from_item` de `dml.rs`.
- Analyzer: arma la `BoundTable` virtual en `bound_from_clause` y resuelve
  el `doc` + `DEFAULT` exprs en `analyzer_stmt::resolve_json_table`.
- Ejecutor: `crates/axiomdb-sql/src/json_table.rs` compila las PATH una
  sola vez y materializa con un walker recursivo estilo MariaDB.
  - First-FROM: `execute_select_json_table_source` en `select_core.rs`.
  - JOIN right-side: nuevo arm en `select_joins_ctx.rs` (rechaza `doc`
    correlacionado con `NotImplemented` que apunta a 11.20d).

### Validacion

- `cargo test -p axiomdb-sql --test integration_json_table` — 16/16 OK.
- `cargo test -p axiomdb-sql` — sin regresion.
- `cargo clippy -p axiomdb-sql --lib -- -D warnings` — limpio.
- `cargo fmt --check` — limpio.
- `tools/wire-test.py` — 367/367 OK (incluye 6 nuevas aserciones JSON_TABLE).

### Diferencias vs PostgreSQL / MariaDB

- `doc` correlacionado a columnas del FROM izquierdo (`JSON_TABLE(u.tags, ...)`
  en un JOIN) se rechaza con error explicito pidiendo 11.20d. PG/MariaDB
  lo soportan nativamente via semantica LATERAL.
- `JSON_TABLE` como primera entrada del `FROM` combinada con `JOIN` tambien
  se rechaza; PG/MariaDB tratan `JSON_TABLE` como fuente simetrica.
- `NESTED PATH` no soportado todavia.

---

## 11.20b — `JSON_TABLE` single-level `NESTED PATH` (2026-04-13)

### Resumen

`NESTED PATH` dentro de `COLUMNS(...)` con:
- LEFT-OUTER NULL padding cuando el array hijo esta vacio o la clave falta.
- Ordinality independiente por nivel (el `FOR ORDINALITY` del padre incrementa
  por cada parent match; el del hijo se resetea a 1 por cada padre).
- Sintaxis `NESTED [PATH] '$.x' COLUMNS (...)` (palabra `PATH` opcional).

### Componentes

- AST: nueva variante `JsonTableColumn::Nested { path, columns }`.
- Parser: dispatch `NESTED` en `parse_column_def` + recursion via misma
  funcion.
- `JsonTableSpec`: compilacion DFS con `slot` fijo por leaf y
  `slot_range: (usize,usize)` por `Nested`; `total_slots` en el spec.
- Executor: `materialize_json_table` allocates template `Vec<Value>` de
  `total_slots` NULLs por parent match, fills leaves, y expande NESTED
  (clone del template por child match; empty child list → emit template
  como LEFT-OUTER pad).
- `column_defs_for_ast` ahora recursivo con flag `inside_nested=true`
  propagado para marcar nullable en el schema flat.

### Restricciones 11.20b → deferidas a 11.20c

- Multi-sibling NESTED dentro del mismo `COLUMNS(...)` → `NotImplemented`.
- Anidamiento profundidad ≥ 2 (NESTED dentro de NESTED) → `NotImplemented`.

### Validacion

- `cargo test -p axiomdb-sql --test integration_json_table_nested` — 11/11 OK.
- `cargo test -p axiomdb-sql --test integration_json_table` — 16/16 OK (sin regresion).
- `cargo clippy -p axiomdb-sql --lib -- -D warnings` — limpio.
- `cargo fmt --check` — limpio.
- `tools/wire-test.py` — 370/370 OK (3 aserciones nuevas NESTED).

### Diferencias vs PostgreSQL

- PG emite un plan-tree aplanado en planner time (`parse_jsontable.c`
  + `nodeTableFuncscan.c`). AxiomDB usa modelo recursivo MariaDB-style
  porque mapea directo al layout DFS de slots sin IR plan-tree.
- PG exige `PATH` despues de `NESTED`; AxiomDB tambien acepta la forma
  corta `NESTED '<path>' COLUMNS(...)` (MariaDB parity).

---

## 11.20c — multi-sibling + multi-level `NESTED PATH` (2026-04-13)

### Resumen

Lift de los dos guards 11.20b:
- **Multi-sibling NESTED**: dos o más `NESTED PATH` en la misma `COLUMNS(...)`
  → UNION semantics (cada sibling emite sus filas con los otros siblings en
  NULL).
- **Multi-level NESTED**: `NESTED` dentro de `NESTED` hasta profundidad 32.

### Componentes

- `compile_columns_recursive`: borrado `depth >= 1` y `nested_count > 1`;
  agregado defensive `depth > 32 → error`.
- `materialize_json_table` + `fill_leaf_children` (11.20b) colapsan en
  `emit_rows_rec(cols, node, template, level_ord, ...)`:
  1. Fill leaves (Regular / Ordinality / Exists) in-place sobre template.
  2. Si no hay NESTED siblings → push template y return.
  3. Por cada NESTED sibling: walk child path, emit UNION — `max(1, |hijos|)`
     filas por sibling; siblings restantes quedan NULL (template init).
- El mismo `emit_rows_rec` expresa multi-level (recursion sobre children)
  y multi-sibling (iteracion sobre hermanos NESTED) en una sola funcion.

### Semantica UNION confirmada

Para parent con prices=[10,20] y tags=["a","b","c"]:
- sibling prices → 2 filas con tag=NULL
- sibling tags   → 3 filas con price=NULL
- Total: 5 filas (no 2×3=6 — no es cartesiano entre siblings).

Dos siblings vacios → 2 filas LEFT-OUTER pad (una por sibling). PG + MariaDB parity.

### Validacion

- `cargo test -p axiomdb-sql --test integration_json_table_multi` — 10/10 OK.
- `cargo test -p axiomdb-sql --test integration_json_table_nested` — 9/9 OK (los 2 tests "deferred" quitados).
- `cargo test -p axiomdb-sql --test integration_json_table` — 16/16 OK (sin regresion).
- `cargo clippy -p axiomdb-sql --lib -- -D warnings` — limpio.
- `cargo fmt --check` — limpio.
- `tools/wire-test.py` — 372/372 OK (2 aserciones nuevas 11.20c).

### Nota de diseño

Una sola funcion recursiva expresa todos los casos (flat, single-NESTED,
multi-sibling, multi-level, mixto). No hay plan-tree IR separado (modelo
MariaDB). PG usa `JsonTableSiblingJoin` node; AxiomDB lo inlinea como
iteracion sobre siblings en `emit_rows_rec`.

---

## Subfase 11.20d1 — WRAPPER / QUOTES / PASSING

Cerrada 2026-04-13.

### Gramatica aceptada

```
JSON_TABLE(doc, row_path
    [ PASSING expr AS name [, expr AS name]* ]
    COLUMNS (
        col_name TYPE PATH '<jsonpath>'
            [ WITH [CONDITIONAL|UNCONDITIONAL] [ARRAY] WRAPPER
            | WITHOUT [ARRAY] WRAPPER ]
            [ KEEP | OMIT QUOTES [ON SCALAR STRING] ]
            [ ON EMPTY ] [ ON ERROR ],
        ...
    )
) [AS alias]
```

Orden clausulas SQL:2016 / PG / Oracle: `PATH → WRAPPER → QUOTES →
ON EMPTY → ON ERROR`.

### Decision arquitectural — migracion a motor JSONPath completo

Se elimina el walker legacy (`PathStepOwned` + `parse_restricted_path` +
`walk_path_owned` + step_* helpers — ~220 LoC). JSON_TABLE usa ahora el
motor `parse_jsonpath` / `execute_jsonpath_owned_env` de
`eval::functions::json`. Razones:

1. PASSING `$var` requiere un evaluador con tabla de variables; duplicar
   en dos walkers invita divergencia.
2. El 11.21* (filtros, `.size()`, `.type()`, aritmetica) ya vive en el
   motor completo; usuarios esperan el mismo dialecto.
3. Sin perdida de perf en paths planos — ambos walkers ejecutan
   `Key`/`Index` con el mismo costo.

### Engine extension — `FilterSide::Var`

Nueva variante en `eval::functions::json::FilterSide`. Parser de filtros
acepta `$name`, termina en whitespace u operador aritmetico. Resolucion
contra `PassingEnv` (`HashMap<String, serde_json::Value>`). Variable no
declarada → `None` → filtro falso en esa fila (parity PG lax mode). Cada
entry point publico del motor tiene gemelo `_env`:

- `execute_jsonpath_env(root, steps, env) -> Vec<&Value>`
- `execute_jsonpath_owned_env(root, steps, env) -> Vec<Value>`

Entry points no-`_env` son shims con env vacio, cero breakage en
11.19b/c, 11.21a/b/c, `@?`, `@@`, `jsonb_path_*`.

### Parser helpers extraidos

`parser::sql_json_common` module nuevo con 3 fns reutilizables:
`parse_optional_wrapper`, `parse_optional_quotes`,
`parse_optional_passing`. JSON_TABLE las llama; el parser inline de
`SqlJsonQuery` (11.19b/c) puede migrarse tambien en una refactor
posterior sin cambio de comportamiento.

### WRAPPER — tres modos

- `WITHOUT` (default): match unico → pass-through; multi-match → ON ERROR.
- `WITH UNCONDITIONAL ARRAY`: siempre envuelve en `[...]` (hasta
  match vacio → `[]`, pero en JSON_TABLE `empty → ON EMPTY` se dispara
  primero).
- `WITH CONDITIONAL ARRAY`: single match que ya es array → unwrap;
  cualquier otro caso → envuelve. Parity PG.

### QUOTES — solo TEXT

`OMIT QUOTES [ON SCALAR STRING]` sobre columna TEXT cuyo hit es un
string escalar → emite `Value::Text(raw)` sin las comillas JSON
circundantes. Sobre cualquier otra combinacion el render normal de
`serde_to_value_typed` aplica. Parser rechaza `OMIT QUOTES` sobre
columnas non-TEXT (`ParseError` con mensaje explicito).

### PASSING lifecycle

- Parse: `parse_optional_passing` produce `Vec<(Expr, String)>`.
- Analyzer: expresiones visibles para binding como cualquier expr de
  nivel FROM. Para 11.20d1 no pueden referenciar columnas outer
  (correlacion en 11.20d3).
- Compile: `compile_json_table` verifica nombres unicos
  case-insensitive y los guarda en `JsonTableSpec.passing`.
- Materialize: `materialize_json_table` evalua cada expr una vez
  (`outer_row = &[]` para first-FROM), JSON-ifica via
  `value_to_serde_for_env`, construye `PassingEnv`, lo threads en:
  - `execute_jsonpath_owned_env(doc, row_path, &env)` — row path
  - `emit_rows_rec(..., &env, ...)` — cada nivel
  - `materialize_regular` / `materialize_exists` — cada column path
  - walk del child_path de cada NESTED sibling.

### Gotchas

- `FilterSide::Var` en `parse_atom` se reconoce ANTES del branch literal,
  si no `$` se tragaria como parte de un literal generico.
- `OMIT QUOTES` no aplica cuando WRAPPER envolvio en `[...]` — la JSON
  value resultante es array, no string; `OMIT` silenciosamente no-op,
  parity con PG (no es error).
- `value_to_serde_for_env` encode JSONB via `JsonbDecoder::decode`, JSON
  via `serde_json::from_str`. Tipos exoticos (Timestamp/Uuid/Bytes/
  Decimal) renderizan como string via `format!("{other:?}")` — caso
  rarisimo; el driver MySQL nunca entrega esos tipos como PASSING
  expr literales.

### Validacion

- `cargo test -p axiomdb-sql --test integration_json_table_wrapper` — 15/15 OK.
- `cargo test -p axiomdb-sql --test integration_json_table` — 16/16 OK.
- `cargo test -p axiomdb-sql --test integration_json_table_nested` — 9/9 OK.
- `cargo test -p axiomdb-sql --test integration_json_table_multi` — 10/10 OK.
- `cargo test -p axiomdb-sql --test integration_sql_json_query` — 35/35 OK.
- `cargo test -p axiomdb-sql --test integration_sql_json_query_wrapper_quotes` — 21/21 OK.
- `cargo test -p axiomdb-sql --test integration_sql_json_passing` — 7/7 OK.
- `cargo test -p axiomdb-sql --test integration_jsonb` — 25/25 OK.
- `cargo test -p axiomdb-sql --test integration_jsonb_operators` — 12/12 OK.
- `cargo clippy --workspace -- -D warnings` — limpio.
- `cargo fmt --check` — limpio.
- `tools/wire-test.py` — 373/373 OK (`[11.20d1 JSON_TABLE wrapper/quotes/passing]` roundtrip).

### Pendiente 11.20d

- 11.20d4: JSON_TABLE como source UPDATE/DELETE. MERGE diferido hasta
  que MERGE mismo aterrice.

## Subfase 11.20d2 — JSON_TABLE primer FROM + CROSS/OUTER APPLY

### Nuevo

- `JSON_TABLE(...) AS j JOIN t ON ...` — JSON_TABLE como **primer**
  FROM combinado con JOINs (INNER / LEFT / subquery / otro
  JSON_TABLE).
- `CROSS APPLY src` y `OUTER APPLY src` como azúcar parser de
  `INNER JOIN src ON TRUE` y `LEFT JOIN src ON TRUE` respectivamente.
  Son genéricos — funcionan con tablas normales, subqueries y
  JSON_TABLE. Non-correlated: `doc` que referencia columnas outer
  sigue rechazado con `NotImplemented 11.20d3`.

### Implementación

- `lexer.rs`: nuevo `Token::Apply` (`APPLY`, ignore ASCII case).
- `parser/dml.rs::parse_join_clauses`: desugar en tiempo de parseo
  — `CROSS APPLY` → `JoinType::Inner + ON TRUE`, `OUTER APPLY`
  → `JoinType::Left + ON TRUE`. Disambiguacion por peek2: si tras
  `CROSS` viene `APPLY` → APPLY; si viene `JOIN` → CROSS JOIN
  existente. `Outer` al top-level del match solo dispara cuando le
  sigue `Apply` — `LEFT/RIGHT/FULL [OUTER] JOIN` sigue consumiendo
  `Outer` dentro de esas ramas sin interferencia. APPLY no acepta
  `ON` ni `USING` — cualquier clausula posterior da parse error
  explicito.
- `executor/select_joins_ctx.rs`: `execute_select_with_joins_ctx`
  refactorizado en wrapper delgado que resuelve la tabla y delega
  a nuevo `execute_select_with_joins_first_materialized(stmt,
  first_source, first_rows, exec_ctx, conn_txn, ctx)` que contiene
  el bucle de JOINs. Reutilizable desde el path JSON_TABLE-first.
- `executor/select_core.rs::execute_select_json_table_source`:
  cuando `!stmt.joins.is_empty()`, materializa el JSON_TABLE como
  source 0 y delega a
  `execute_select_with_joins_first_materialized` con
  `ExecutionContext::new(storage, txn, &temp_bloom, None)` y un
  `SessionContext::new()` temporal (misma receta que
  `execute_select_derived`). `doc_has_column_refs` de guardia
  antes de delegar — referencias outer imposibles en primer FROM
  por definicion, pero se rechaza explicitamente con mensaje
  11.20d3 por robustez.
- No nuevas variantes AST — `CROSS/OUTER APPLY` se pierden al
  parsear (surface form no se preserva). Fuera de alcance por spec.

### Cobertura

- `tests/integration_json_table_first_from.rs` — 12 tests:
  JSON_TABLE primer FROM + INNER/LEFT JOIN, JSON_TABLE × JSON_TABLE,
  CROSS APPLY ≡ JOIN ON TRUE, CROSS APPLY non-correlated,
  OUTER APPLY preserva izquierda cuando JSON vacio, CROSS APPLY
  sobre tabla regular, APPLY rechaza `ON`, WHERE/ORDER BY/LIMIT/
  GROUP BY sobre join, NESTED PATH con join.
- `tools/wire-test.py` — 376/376 OK (3 aserciones nuevas
  `[11.20d2 JSON_TABLE first FROM + APPLY]`).
- Regresion 11.20a/b/c/d1 limpia.

### Cross-engine

- **SQL Server / Sybase**: origen historico de `CROSS APPLY` /
  `OUTER APPLY`. Semantica conservada.
- **PostgreSQL**: usa `LATERAL` en lugar de APPLY —
  `CROSS APPLY src` ≡ `CROSS JOIN LATERAL src`; `OUTER APPLY src` ≡
  `LEFT JOIN LATERAL src ON TRUE`. El keyword `LATERAL` queda
  deferido a 11.20d3 (donde aterriza la correlacion real).
- **Oracle 12c+**: soporta ambos (`CROSS APPLY` y `LATERAL`).

### Pendiente 11.20d

- 11.20d4: JSON_TABLE como source UPDATE/DELETE. MERGE diferido
  hasta que MERGE mismo aterrice.

## Subfase 11.20d3 — LATERAL-correlated JSON_TABLE

### Nuevo

- `doc` correlacionado: `CROSS APPLY JSON_TABLE(t.payload, ...)`
  re-materializa JSON_TABLE una vez por fila outer.
- PASSING correlacionado: `PASSING t.col AS var` resuelve `t.col`
  contra cada fila outer; `$var` en row/column/NESTED paths y en
  filtros se substituye con el valor outer correspondiente.
- `LATERAL` keyword aceptado como no-op antes de `JSON_TABLE(...)`
  y antes de subqueries en FROM / right-source de JOIN. Semantica
  LATERAL ya es implicita en AxiomDB para JSON_TABLE; el keyword
  existe por paridad con PG.
- Semantica por join-type:
  - INNER / CROSS JOIN / CROSS APPLY → emit solo matches ON.
  - LEFT JOIN / OUTER APPLY → NULL-pad cuando el doc da 0 rows.
  - RIGHT JOIN / FULL JOIN → `NotImplemented` (PG tambien rechaza;
    outer re-scan ill-defined).

### Implementacion

- `analyzer_stmt.rs`:
  - `resolve_json_table` ahora resuelve `jt.passing` exprs contra
    el mismo scope que `jt.doc`.
  - El loop de joins rutea `FromClause::JsonTable` a
    `resolve_json_table` — antes solo first-FROM JT se resolvia;
    join-side JT quedaba sin bindings (los literales de 11.20d1
    funcionaban por accidente).
- `lexer.rs`: `Token::Lateral` case-insensitive.
- `parser/dml.rs::parse_from_item`: consume `LATERAL` opcional al
  entrar. Cubre `FROM LATERAL X`, `JOIN LATERAL X`,
  `LATERAL (SELECT ...)` naturalmente.
- `json_table.rs::jsontable_is_correlated(jt)`:
  `doc_has_column_refs(doc) ||
   jt.passing.iter().any(|(e,_)| doc_has_column_refs(e))`.
- `executor/select_joins_ctx.rs`: tracker paralelo
  `correlated_jt: Vec<Option<JsonTableSpec>>`. Non-correlated →
  `None` + materializacion one-shot (igual que antes). Correlated
  → `Some(spec)` + `scanned[i] = Vec::new()` como placeholder. El
  combine loop dispatcha segun el tracker.
- `executor/joins.rs::apply_correlated_jt_join`: per-outer-row
  loop. `eval(doc, outer)` → `doc_to_serde` →
  `materialize_json_table(spec, &sj, outer, &mut NoSubquery)` →
  iterar right_rows y evaluar ON. LEFT/OUTER APPLY null-pad al
  final si no hubo matches. RIGHT/FULL: error al principio.
- `executor/select_core.rs::execute_select_json_table_source`:
  first-FROM correlated guard usa `jsontable_is_correlated` y da
  `ParseError` ("correlated JSON_TABLE requires an outer FROM
  source") en lugar del placeholder anterior.

### Cobertura

- `tests/integration_json_table_correlated.rs` — 13 tests nuevos
  (CROSS APPLY basico, OUTER APPLY empty-preserve, INNER con ON,
  LEFT null-pad, PASSING una var, PASSING dos vars en rango,
  correlated NULL doc, correlated + NESTED PATH, LATERAL sobre
  JOIN, LATERAL sobre first-FROM non-correlated, RIGHT/FULL
  rechazados, first-FROM correlated rechazado).
- `tests/integration_json_table.rs::correlated_doc_in_join_is_rejected_11_20a`
  → `correlated_doc_in_join_works_11_20d3` (flipeado a
  assert-success).
- Regresion 11.20a/b/c/d1/d2: limpia.
- Wire smoke 379/379 (3 aserciones nuevas `[11.20d3]`).

### Limitaciones conocidas

- Hash-join / spill optimization NO se aplica a JSON_TABLE
  correlado (siempre nested-loop). Aceptable.
- `PASSING` con subqueries anidados (e.g. `PASSING (SELECT ...)
  AS v`) usa `NoSubquery` runner → no soportado aun.
- `LATERAL` sobre subqueries con correlacion real (no
  JSON_TABLE) queda para una subphase separada — requiere exponer
  outer_scopes al analyzer del derived SELECT.

### Cross-engine

- **PostgreSQL**: `JOIN LATERAL` — AxiomDB matchea para
  JSON_TABLE; LATERAL keyword no-op.
- **SQL Server / Sybase / Oracle 12c+**: `CROSS APPLY` / `OUTER
  APPLY` — AxiomDB iguala.
- **PG** rechaza `RIGHT JOIN LATERAL` y `FULL JOIN LATERAL` por la
  misma razon que nosotros.

### Pendiente 11.20d

- 11.20d4: JSON_TABLE como source UPDATE/DELETE. MERGE diferido
  hasta que MERGE mismo aterrice.
