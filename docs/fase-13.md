# Fase 13 - Advanced PostgreSQL

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
