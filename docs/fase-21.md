# Fase 21 - Advanced SQL

## 21.5f Generated columns - cerrada 2026-04-21

La subfase 21.5f agrega soporte usable para
`GENERATED ALWAYS AS (expr) STORED` en `CREATE TABLE`, con persistencia en el
catalogo y materializacion compartida en todos los caminos de escritura.

### Superficie SQL cerrada

```sql
CREATE TABLE line_items (
    price INT NOT NULL,
    qty   INT NOT NULL,
    total INT GENERATED ALWAYS AS (price * qty) STORED
);

INSERT INTO line_items (price, qty) VALUES (10, 3);          -- total = 30
INSERT INTO line_items (price, qty, total) VALUES (10, 3, DEFAULT);
UPDATE line_items SET qty = 4 WHERE price = 10;              -- total = 40
```

Alcance implementado:

- Parser/AST: `ColumnConstraint::Generated { expr, kind }`.
- Catalogo: `axiom_columns` ahora persiste `generated_expr` y
  `generated_stored` (flags bit6/bit7).
- DDL: `CREATE TABLE` valida que la expresion use solo columnas base ya
  declaradas, no se auto-referencie, no dependa de otras generated columns y
  no use `DEFAULT`, `ON UPDATE`, `AUTO_INCREMENT`, subqueries ni agregados.
- Executor: `materialize_generated_columns()` centraliza la recomputacion y se
  invoca en heap/clustered INSERT, `INSERT ... SELECT`, UPDATE, UPDATE JOIN,
  `ON CONFLICT`, MySQL ODKU y `MERGE`.
- Semantica de escritura: cualquier asignacion directa no-`DEFAULT` a una
  generated column falla; `DEFAULT` significa "recalcular".

### Diferido explicito

- `VIRTUAL` se parsea pero devuelve `DbError::NotImplemented`.
- `ALTER TABLE ... GENERATED` sigue diferido: falta el backfill/rewrite fisico.
- Cadenas de dependencias entre generated columns siguen fuera de alcance.

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-catalog` - paso.
- `cargo test -p axiomdb-sql --test integration_generated_columns` - paso, 19 tests.
- `cargo test -p axiomdb-sql --test integration_lateral_join` - paso, 11 tests.
- `cargo test -p axiomdb-sql` - paso.
- `cargo clippy -p axiomdb-sql -- -D warnings` - paso.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.
- `tools/wire-test.py` - paso, 419/419 assertions.

### Nota de implementacion

La decision clave fue no repartir la logica por cada executor de DML. Las
generated columns se resuelven en un helper unico despues de defaults /
auto_increment y antes de CHECK/FK/indexes/RETURNING. Eso mantiene
consistencia entre caminos normales, conflict-update, `MERGE` y tablas
clustered sin introducir reglas divergentes.
