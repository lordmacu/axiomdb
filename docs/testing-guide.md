# AxiomDB — Guia de tests dirigidos

La regla general es simple:

- corre primero el bin mas pequeno que cubra exactamente el path que tocaste
- agrega bins vecinos solo si el cambio toca helpers compartidos o un path relacionado
- deja `cargo test --workspace` solo para el cierre de subfase/fase

Esto evita pagar compilacion y ejecucion de bins que no dan senal para el cambio actual.

## SQL executor: bins actuales

Los integration tests de `axiomdb-sql` ya no viven en un solo archivo monolitico. El layout actual es:

| Bin | Archivo | Responsabilidad principal | Comando |
|---|---|---|---|
| `integration_executor` | `crates/axiomdb-sql/tests/integration_executor.rs` | CRUD base, `SELECT` simple, DDL basico, transacciones base | `cargo test -p axiomdb-sql --test integration_executor` |
| `integration_executor_joins` | `crates/axiomdb-sql/tests/integration_executor_joins.rs` | JOINs y agregacion base | `cargo test -p axiomdb-sql --test integration_executor_joins` |
| `integration_executor_query` | `crates/axiomdb-sql/tests/integration_executor_query.rs` | `ORDER BY`, `LIMIT`, `DISTINCT`, `CASE`, `INSERT ... SELECT`, `AUTO_INCREMENT` | `cargo test -p axiomdb-sql --test integration_executor_query` |
| `integration_executor_ddl` | `crates/axiomdb-sql/tests/integration_executor_ddl.rs` | `SHOW`, `DESCRIBE`, `TRUNCATE`, `ALTER TABLE` | `cargo test -p axiomdb-sql --test integration_executor_ddl` |
| `integration_executor_ctx` | `crates/axiomdb-sql/tests/integration_executor_ctx.rs` | base ctx path y `strict_mode` | `cargo test -p axiomdb-sql --test integration_executor_ctx` |
| `integration_executor_ctx_group` | `crates/axiomdb-sql/tests/integration_executor_ctx_group.rs` | sorted group-by por ctx | `cargo test -p axiomdb-sql --test integration_executor_ctx_group` |
| `integration_executor_ctx_limit` | `crates/axiomdb-sql/tests/integration_executor_ctx_limit.rs` | coercion de `LIMIT/OFFSET` | `cargo test -p axiomdb-sql --test integration_executor_ctx_limit` |
| `integration_executor_ctx_on_error` | `crates/axiomdb-sql/tests/integration_executor_ctx_on_error.rs` | `on_error` y rollback/savepoint semantics | `cargo test -p axiomdb-sql --test integration_executor_ctx_on_error` |
| `integration_executor_sql` | `crates/axiomdb-sql/tests/integration_executor_sql.rs` | cobertura SQL amplia fuera del path ctx | `cargo test -p axiomdb-sql --test integration_executor_sql` |
| `integration_delete_apply` | `crates/axiomdb-sql/tests/integration_delete_apply.rs` | bulk delete e indexed delete apply paths | `cargo test -p axiomdb-sql --test integration_delete_apply` |
| `integration_insert_staging` | `crates/axiomdb-sql/tests/integration_insert_staging.rs` | transactional INSERT staging | `cargo test -p axiomdb-sql --test integration_insert_staging` |
| `integration_namespacing` | `crates/axiomdb-sql/tests/integration_namespacing.rs` | `CREATE/DROP DATABASE`, `USE`, `SHOW DATABASES` | `cargo test -p axiomdb-sql --test integration_namespacing` |
| `integration_namespacing_cross_db` | `crates/axiomdb-sql/tests/integration_namespacing_cross_db.rs` | resolucion `database.schema.table` y DML/DDL cross-db | `cargo test -p axiomdb-sql --test integration_namespacing_cross_db` |
| `integration_namespacing_schema` | `crates/axiomdb-sql/tests/integration_namespacing_schema.rs` | `CREATE SCHEMA`, `search_path`, `SHOW TABLES` por schema | `cargo test -p axiomdb-sql --test integration_namespacing_schema` |

El harness compartido vive en `crates/axiomdb-sql/tests/common/mod.rs`.

## Como correr un test puntual

```bash
cargo test -p axiomdb-sql --test integration_executor_query test_insert_select_aggregation -- --exact
```

Eso debe ser el punto de partida cuando ya sabes que funcionalidad cambiaste.

## Politica de ejecucion minima

1. Cambiaste un path local y obvio.
   Corre solo el bin tematico correspondiente.
2. Cambiaste un helper compartido o un path usado por bins vecinos.
   Corre el bin principal y los bins directamente relacionados.
3. Cambiaste una superficie compartida del crate o no estas seguro del blast radius.
   Corre `cargo test -p axiomdb-sql --tests`.
4. Solo en cierre de subfase/fase:
   corre `cargo test --workspace`, `cargo clippy --workspace -- -D warnings` y `cargo fmt --check`.

## Regla para agregar tests nuevos

- si la funcionalidad nueva encaja en un bin tematico existente, agrega el test ahi
- no crees un bin nuevo solo porque agregaste un test mas
- crea un bin nuevo solo si:
  - la responsabilidad ya es claramente distinta, o
  - el archivo empezo a mezclar varios paths poco relacionados, o
  - el archivo ya cruzo aproximadamente `~1000` lineas y sigue creciendo

La meta no es tener muchos archivos. La meta es poder correr exactamente lo necesario sin perder contexto semantico.

## Candidatos actuales a futuro split

No hace falta split inmediato para:

- `crates/axiomdb-sql/tests/integration_executor_query.rs`
  porque sigue siendo cohesivo alrededor de shape/query semantics
- `crates/axiomdb-sql/tests/integration_index_only.rs`
  porque sigue siendo cohesivo alrededor de `IndexOnlyScan`
