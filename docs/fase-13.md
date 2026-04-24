# Fase 13 - Advanced PostgreSQL

## 13.12 Statement-level triggers - cerrada 2026-04-23

La subfase 13.12 cierra el primer corte real de triggers SQL en AxiomDB, pero
deliberadamente como **validation-trigger MVP** y no como sistema general de
triggers. El recorte entregado sirve para validacion por sentencia, sobre todo
en el caso contable de batch inserts que deben revisarse una sola vez despues
del DML completo.

### Superficie SQL cerrada

Soportado:

- `CREATE TRIGGER trg AFTER INSERT|UPDATE|DELETE ON tabla FOR EACH STATEMENT AS SELECT ...`
- `DROP TRIGGER trg ON tabla`
- `SHOW CREATE TRIGGER trg ON tabla`
- triggers `AFTER` sobre tablas base
- firing una sola vez por sentencia DML tope (`INSERT`, `UPDATE`, `DELETE`)
- rollback de la sentencia outer cuando el `SELECT` de validacion devuelve filas

Fuera de alcance en este corte:

- `BEFORE`
- `FOR EACH ROW`
- `WHEN`, `SIGNAL`, transition tables `OLD/NEW TABLE`
- cuerpos multi-statement o procedurales
- triggers sobre views o materialized views
- recursion o disparo desde DML interno de mantenimiento

### Ajustes tecnicos de cierre

- Catalogo: `TableDef` ahora persiste `triggers: Vec<TriggerDef>` con nombre,
  evento, SQL cuerpo y ordinal de creacion; el formato on-disk de tabla sube a
  v6 conservando lectura compatible de filas viejas sin triggers.
- Parser/AST: se agregan `CREATE TRIGGER`, `DROP TRIGGER` y
  `SHOW CREATE TRIGGER`; `BEFORE` y `FOR EACH ROW` quedan rechazados
  explicitamente como `NotImplemented`.
- Ejecutor DDL: create/drop trigger actualizan la metadata owned-by-table y
  bump de `schema_version`.
- Ejecutor DML: `INSERT`, `UPDATE` y `DELETE` corren dispatch compartido post
  sentencia; si el body `SELECT` devuelve filas, la sentencia outer falla con
  `TriggerValidationFailed` y se deshace bajo el rollback por sentencia ya
  existente.
- Contexto trigger: el body almacenado se reparsea al disparar y recibe
  `@@trigger_name`, `@@trigger_table`, `@@trigger_event` y
  `@@trigger_row_count` via sustitucion SQL acotada.
- Wire/read-only path: `SHOW CREATE TRIGGER` quedo soportado tambien por la
  ruta read-only del handler MySQL para no caer en el viejo fallback
  `NotSupported`.

### Cobertura agregada

- `crates/axiomdb-sql/tests/integration_statement_triggers.rs`
- `crates/axiomdb-sql/tests/integration_ddl_parser.rs`
- bloque wire `[13.12 statement triggers]` en `tools/wire-test.py`

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --test integration_ddl_parser --test integration_statement_triggers` - paso.
- `python3 tools/wire-test.py` - paso, 461/461 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

## 13.6 Non-blocking ALTER TABLE - cerrada 2026-04-23

La subfase 13.6 cierra el primer corte real de `ALTER TABLE` no bloqueante en
AxiomDB sin intentar online DDL completo con replay de writes. El recorte
entregado es **shadow heap rebuild + atomic cutover** para operaciones de
reescritura de una sola columna sobre heap tables: `ADD COLUMN`, `DROP COLUMN`
y `MODIFY COLUMN`.

### Superficie SQL cerrada

Soportado:

- `ALTER TABLE heap_table ADD COLUMN ...`
- `ALTER TABLE heap_table DROP COLUMN ...`
- `ALTER TABLE heap_table MODIFY COLUMN ...`
- copy largo sobre shadow heap root con lecturas concurrentes todavia vivas
- cutover atomico de root + columnas + indexes en una ventana exclusiva corta
- rechazo de writes concurrentes a la tabla objetivo via `LockTimeout`

Fuera de alcance en este corte:

- replay WAL-delta para permitir writers concurrentes
- ALTERs multi-operacion en la ruta no bloqueante
- heap rebuild no bloqueante para `ADD PRIMARY KEY`
- tablas clustered
- jobs async de migracion o progress reporting

### Ajustes tecnicos de cierre

- Ejecutor SQL: nuevo plan `NonBlockingHeapAlterPlan` que deriva el esquema
  destino, materializa filas en un shadow heap root y reconstruye los indexes
  secundarios necesarios antes del publish final.
- Handler/shared DB: `SharedDatabase` ahora mantiene un registro
  `table_rewrites` por `table_id`. Mientras una tabla esta en rewrite, DML y
  DDL mutantes sobre esa tabla fallan temprano con `LockTimeout`.
- Coordinacion: el path especial del handler MySQL usa `catalog_lock.read()`
  durante la copia larga y `catalog_lock.write()` solo para el swap final,
  evitando que los lectores normales queden serializados detras de todo el
  rewrite.
- Cutover: el swap actualiza root de tabla, filas de columnas y definiciones de
  indice/FK segun la operacion, y manda las paginas viejas a deferred free solo
  despues del publish exitoso.
- Semantica autocommit: la ruta especial replica el commit/rollback implicito
  del executor normal para que el resultado sea visible por wire inmediatamente
  tras un `ALTER TABLE` exitoso.

### Cobertura agregada

- `crates/axiomdb-sql/tests/integration_executor_ddl.rs`
- `crates/axiomdb-network/tests/integration_concurrency.rs`
- bloque wire `[13.6 non-blocking alter table]` en `tools/wire-test.py`

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --test integration_executor_ddl` - paso.
- `cargo test -p axiomdb-network --test integration_concurrency` - paso.
- `python3 tools/wire-test.py` - paso, 457/457 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

## 13.5 Covering indexes - cerrada 2026-04-23

La subfase 13.5 cierra el soporte real de covering indexes sobre heap tables.
`INCLUDE (...)` ya existia en parser/catalogo desde `6.13`, pero todavia no
guardaba los valores incluidos dentro de la hoja secundaria; el planner podia
aceptar la sintaxis, pero los read-paths seguian dependiendo de la heap row
para proyectar columnas no-key. El cierre de 13.5 completa ese contrato.

### Superficie SQL cerrada

Soportado:

- `CREATE INDEX ... ON heap_table (key_cols...) INCLUDE (cover_cols...)`
- `SELECT` cubiertos donde las columnas proyectadas viven entre key columns e
  `include_columns`
- mantenimiento correcto del payload `INCLUDE` en `INSERT`, `UPDATE`,
  `DELETE`, rebuilds y paths batch
- compatibilidad con `UNIQUE ... INCLUDE (...)` en probes logicos como FK
  parent lookup

Fuera de alcance en este corte:

- covering indexes sobre clustered tables
- nuevo cost model especifico para preferir aggressively covering scans
- cambio de metadata/catalogo adicional para versionar layout de index leaf

### Ajustes tecnicos de cierre

- Formato fisico: las entradas secundarias heap ahora persisten
  `logical_key ++ include_payload ++ optional_rid_suffix`, manteniendo la
  buscabilidad por prefijo del logical key.
- Planner: `index_covers_query(...)` ya cuenta `include_columns` como cobertura
  real y `IndexOnlyScan` arrastra `n_include_cols` para poblar columnas no-key.
- Ejecutor: `select_ctx` decodifica key + include payload desde la entrada de
  indice; si encuentra una entrada legacy sin payload, hace fallback seguro a
  heap row read.
- Mantenimiento: inserts, batch inserts, updates y deletes generan/borran la
  nueva forma fisica; para no romper compatibilidad con entradas pre-13.5,
  deletes tambien intentan borrar la forma legacy sin payload.
- Logical-key probes: unique checks, FK parent validation y otros lookups sobre
  secundarios heap cambiaron de exact-key lookup a prefix scan por logical key,
  porque el key fisico ya no coincide byte-a-byte con el key de busqueda.

### Cobertura agregada

- `crates/axiomdb-sql/tests/integration_index_only.rs`
- `crates/axiomdb-sql/tests/integration_fk.rs`
- `crates/axiomdb-sql/tests/integration_executor_ddl.rs`
- bloque wire `[13.5 covering indexes]` en `tools/wire-test.py`

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --test integration_index_only --test integration_fk --test integration_executor_ddl` - paso.
- `python3 tools/wire-test.py` - paso, 453/453 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

## 13.1 Materialized views - cerrada 2026-04-22

La subfase 13.1 cierra el primer corte real de materialized views en AxiomDB
sin bloquearse en `CREATE VIEW`. El recorte elegido fue tratarlas como una
**relacion fisica administrada por catalogo**: se crean con
`CREATE MATERIALIZED VIEW ... AS SELECT ...`, guardan su SQL origen en
catalogo, y `REFRESH MATERIALIZED VIEW` hace un rebuild completo del contenido.

### Superficie SQL cerrada

Soportado:

- `CREATE MATERIALIZED VIEW name AS SELECT ...`
- `CREATE MATERIALIZED VIEW IF NOT EXISTS ...`
- `REFRESH MATERIALIZED VIEW name`
- `DROP MATERIALIZED VIEW [IF EXISTS] name [, ...]`

Fuera de alcance en este corte:

- views logicas normales (`CREATE VIEW`)
- `REFRESH ... CONCURRENTLY`
- refresh incremental
- dependencia/catalogo de invalidacion fina sobre tablas base

### Ajustes tecnicos de cierre

- Catalogo: `TableDef` ahora persiste `relation_kind` y `defining_query` en un
  trailer v5 backward-compatible; filas legacy siguen decodificando como
  `RelationKind::Table`.
- Writer: `CatalogWriter::create_relation_with_options(...)` generaliza la
  creacion de relaciones sin duplicar el path de tablas normales.
- Parser/AST: nuevos statements `CreateMaterializedView`,
  `RefreshMaterializedView` y `DropMaterializedView`, con captura del SQL
  original del `SELECT` para persistirlo como defining query.
- Ejecutor: `CREATE MATERIALIZED VIEW` reutiliza el path de CTAS para
  materializar filas y crear columnas inferidas; `REFRESH` reparsea y reanaliza
  el SQL guardado, materializa primero, luego hace `TRUNCATE` y recarga el
  `TableDef` actualizado antes de reinsertar filas.
- Metadata visible: `SHOW FULL TABLES`, `SHOW CREATE TABLE` e
  `information_schema.TABLES` reportan `MATERIALIZED VIEW` en vez de
  `BASE TABLE`.

### Cobertura agregada

- `crates/axiomdb-sql/tests/integration_materialized_views.rs`
- `crates/axiomdb-sql/tests/integration_show_full.rs`
- roundtrip de catalogo en `crates/axiomdb-catalog/src/schema.rs`
- bloque wire `[13.1 materialized views]` en `tools/wire-test.py`

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --test integration_materialized_views --test integration_show_full` - paso.
- `tools/wire-test.py` - paso, 445/445 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

## 13.4 LISTEN / NOTIFY - cerrada 2026-04-23

La subfase 13.4 cierra el primer corte real de pub-sub SQL en AxiomDB sin
forzar server-push sobre el wire MySQL. El recorte elegido fue
pull-based y transaccionalmente seguro: las sesiones pueden suscribirse con
`LISTEN`, emitir eventos con `NOTIFY`, limpiar suscripciones con `UNLISTEN`, y
consumir su cola con `SHOW NOTIFICATIONS`.

### Superficie SQL cerrada

Soportado:

- `LISTEN channel`
- `UNLISTEN channel`
- `UNLISTEN *`
- `NOTIFY channel`
- `NOTIFY channel, 'payload'`
- `SHOW NOTIFICATIONS`

Fuera de alcance en este corte:

- server-push asincrono hacia clientes idle
- frames o protocolo estilo PostgreSQL
- `pg_notify(...)`
- filtros/predicados de notificacion (`13.15`)
- persistencia de suscripciones o colas a traves de restart

### Ajustes tecnicos de cierre

- Lexer/parser/AST: nuevos statements `LISTEN`, `UNLISTEN`, `NOTIFY` y
  `SHOW NOTIFICATIONS`.
- Runtime SQL: `SessionContext` ahora mantiene un `session_id` estable, una
  cola FIFO de notificaciones pendientes y el buffer transaccional de `NOTIFY`
  aun no committeados.
- Broker: nuevo broker in-process compartido, indexado por canal normalizado,
  que enruta eventos entre sesiones activas sin persistencia en disco.
- Semantica transaccional: `NOTIFY` se encola dentro de la transaccion actual y
  solo se publica tras `COMMIT`; `ROLLBACK` y `ROLLBACK TO SAVEPOINT` descartan
  eventos pendientes posteriores al punto revertido.
- Lifecycle wire: `COM_RESET_CONNECTION`, `COM_CHANGE_USER` y disconnect
  limpian suscripciones, cola visible y pendientes transaccionales.
- Read path: `SHOW NOTIFICATIONS` drena la cola real tambien en la ruta
  read-only compartida del handler MySQL; el fix final de la subfase fue
  corregir ese path para que no devolviera filas vacias hardcodeadas.

### Cobertura agregada

- `crates/axiomdb-sql/tests/integration_listen_notify.rs`
- `crates/axiomdb-network/tests/integration_listen_notify.rs`
- bloque wire `[13.4 listen notify]` en `tools/wire-test.py`

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --test integration_listen_notify` - paso.
- `cargo test -p axiomdb-network --test integration_listen_notify` - paso.
- `python3 tools/wire-test.py` - paso, 451/451 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

## 13.2 Window functions - cerrada 2026-04-23

La subfase 13.2 cierra el primer corte real de window functions en AxiomDB con
un MVP deliberadamente acotado: **ranking windows** sobre filas ya
materializadas, sin intentar resolver en la misma entrega navegacion entre
filas, frames ni aggregates sobre ventana.

### Superficie SQL cerrada

Soportado:

- `ROW_NUMBER() OVER ( [PARTITION BY ...] ORDER BY ... )`
- `RANK() OVER ( [PARTITION BY ...] ORDER BY ... )`
- `DENSE_RANK() OVER ( [PARTITION BY ...] ORDER BY ... )`
- uso como expresion tope del `SELECT` en queries no agrupadas y sin joins

Fuera de alcance en este corte:

- `LAG`, `LEAD`, `FIRST_VALUE`, `LAST_VALUE`, `NTILE`
- aggregate windows como `SUM(...) OVER (...)`
- `OVER` con frames (`ROWS`, `RANGE`, `GROUPS`)
- named windows (`WINDOW w AS (...)`)
- window functions en `WHERE`, `GROUP BY`, `HAVING`, `ORDER BY`, `JOIN ON`
- windows anidadas dentro de otras expresiones

### Ajustes tecnicos de cierre

- Lexer/parser/AST: `OVER`, `PARTITION` y nuevo nodo `Expr::Window` con
  `WindowSpec { partition_by, order_by }`.
- Analyzer: resolucion de columnas dentro de `PARTITION BY` / `ORDER BY` de la
  ventana y validacion del contrato MVP; combinaciones fuera de alcance ahora
  fallan temprano con mensajes explicitos.
- Ejecutor: nueva proyeccion con soporte de ventanas sobre filas ya
  materializadas. El engine calcula claves de particion/orden, ordena por
  spec de ventana y decora cada fila con `ROW_NUMBER`, `RANK` o `DENSE_RANK`
  antes del `ORDER BY` final de la query.
- Metadata: `expr_column_name` e inferencia de tipos ya publican nombres
  `row_number` / `rank` / `dense_rank` y tipo `BIGINT` para columnas ventana.

### Cobertura agregada

- `crates/axiomdb-sql/tests/integration_window_functions.rs`
- regresion de parser/analyzer en `cargo test -p axiomdb-sql`
- bloque wire `[13.2 window functions]` en `tools/wire-test.py`

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --test integration_window_functions` - paso.
- `python3 tools/wire-test.py` - paso, 446/446 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

## 13.3 Generated columns - cerrada 2026-04-23

La subfase 13.3 no requirio una segunda implementacion grande: el repo ya
tenia el slice real de generated columns por `21.5f`. El cierre correcto fue
alinear Fase 13 con ese contrato real y dejar de sugerir una paridad mas amplia
de la que hoy existe en codigo.

### Superficie SQL cerrada

Soportado:

- `CREATE TABLE ... col TYPE GENERATED ALWAYS AS (expr) STORED`
- persistencia de metadata en catalogo (`generated_expr`, `generated_stored`)
- recomputacion en write-paths ya soportados (`INSERT`, `UPDATE`, `ON
  CONFLICT`, ODKU, `MERGE`, y variantes ya cubiertas por la suite existente)
- rechazo explicito de escrituras directas salvo `DEFAULT`

Fuera de alcance en este corte:

- `VIRTUAL` generated columns
- `ALTER TABLE ... ADD/ALTER COLUMN ... GENERATED`
- expresiones con subqueries, windows o aggregates en generated columns
- `GENERATED ALWAYS AS IDENTITY` (sigue en `24.1c`)

### Ajustes tecnicos de cierre

- El cierre de `13.3` se apoya en la implementacion ya existente en
  `executor/ddl_create_table.rs`, `executor/insert_helpers.rs` y la metadata de
  `axiom_columns`; no hizo falta abrir un segundo path semantico.
- El smoke wire de Fase 13 ya prueba tanto el happy path de una columna
  `STORED` que se materializa/recomputa como el rechazo explicito de
  `VIRTUAL`, para que el contrato visible de la subfase quede fijado sin
  depender solo del hito `21.5f`.
- La documentacion del roadmap ahora distingue claramente entre soporte real y
  follow-ups deferidos, en vez de mezclar `STORED` implementado con `VIRTUAL`
  aun no soportado.

### Cobertura agregada

- `crates/axiomdb-sql/tests/integration_generated_columns.rs`
- bloque wire `[13.3 generated columns]` en `tools/wire-test.py`
- notas de arquitectura y lessons alineadas con el cierre de Fase 13

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --test integration_generated_columns` - paso.
- `python3 tools/wire-test.py` - paso, 448/448 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.
