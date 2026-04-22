# Fase 21 - Advanced SQL

## 21.11 Query hints - cerrada 2026-04-22

La subfase 21.11 cierra un MVP acotado de optimizer hints sobre `SELECT`.
La implementacion no intenta replicar todo el framework de MariaDB/MySQL;
resuelve el corte pragmatico que faltaba en AxiomDB: capturar `/*+ ... */`
antes de que el lexer descarte los block comments, llevar esos hints al AST
y darles efecto real en planner / join execution donde hoy ya existe una base
tecnica reutilizable.

### Superficie SQL cerrada

```sql
SELECT /*+ INDEX(users idx_users_email) */ id
FROM users
WHERE email = 'alice@example.com';

SELECT /*+ HASH_JOIN */ *
FROM t
JOIN u ON t.id = u.t_id;

SELECT /*+ PARALLEL(4) */ id
FROM users
WHERE email = 'alice@example.com';
```

Alcance implementado:

- Lexer/parser/AST: nuevo `Token::OptimizerHint(String)` y
  `SelectStmt.hints: Vec<SelectHint>`; los hints soportados en esta subfase
  son `INDEX(table index)`, `HASH_JOIN` y `PARALLEL(n)`.
- Compatibilidad de comentarios: `/*+ ... */` ya no se pierde en el `skip`
  general de block comments; los comentarios normales `/* ... */` y los
  version comments `/*! ... */` conservan su comportamiento previo.
- Planner: en SELECT de una sola tabla, `INDEX(table index)` puede
  re-planificar contra un index nombrado y usarlo si el predicado es
  compatible; si el index existe pero no aplica, se conserva el plan normal.
- Join executor: `HASH_JOIN` fuerza el camino hash cuando el join ya es un
  equijoin soportado por la implementacion existente; si no lo es, cae al
  nested-loop normal sin cambiar resultados.
- EXPLAIN / wire: `EXPLAIN` expone el index hinted elegido y deja visible el
  hint de hash join; `PARALLEL(n)` se acepta como advisory-only y se refleja
  en `Extra`.

### Diferido explicito

- Framework completo estilo MariaDB (`QB_NAME`, `NO_*`, `JOIN_ORDER`,
  `INDEX_MERGE`, etc.).
- Hints en `UPDATE` / `DELETE` / `INSERT` / `MERGE`.
- Garantias fuertes de paralelismo para `PARALLEL(n)`.
- `INDEX(...)` sobre joins multi-tabla; el MVP actual lo limita
  explicitamente al planner single-table.

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --lib --test integration_dml_parser --test integration_query_hints` - paso.
- `cargo test -p axiomdb-sql --test integration_query_hints --test integration_expression_index --test integration_executor_joins` - paso.
- `tools/wire-test.py` - paso, 427/427 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

### Nota de implementacion

El error facil aqui era atacar `21.11` como si el gap fueran los viejos
modifiers tipo `HIGH_PRIORITY` o `STRAIGHT_JOIN`. Eso ya estaba cerrado. El
trabajo real era mas bajo nivel: mientras `lexer.rs` siguiera descartando
todo `/* ... */`, ningun optimizer hint podia llegar al parser. Una vez
resuelto ese punto de entrada, el MVP correcto fue conectar hints solamente a
los caminos que ya existen y son seguros hoy: planner single-table,
heuristica hash-vs-nested-loop, y metadata advisory en `EXPLAIN`.

## 21.20 CHECKPOINT - cerrada 2026-04-22

La subfase 21.20 cierra el statement administrativo SQL `CHECKPOINT`.
La implementacion reutiliza el motor de checkpoint que ya existia en WAL y
storage, pero ahora lo expone por SQL con el contrato correcto: persiste un
LSN de checkpoint durable, no rota el WAL, y rechaza la operacion mientras
haya cualquier transaccion activa.

### Superficie SQL cerrada

```sql
CHECKPOINT;
```

Alcance implementado:

- Parser/AST: nuevo `Stmt::Checkpoint` con soporte directo en lexer/parser y
  cobertura de parser en `integration_ddl_parser`.
- WAL/admin: `TxnManager::checkpoint(storage)` encapsula el guard de
  `active_set` y delega en `Checkpointer::checkpoint(...)` sin mezclarlo con
  `rotate_wal`.
- Executor: `CHECKPOINT` corre como statement real tanto en la ruta legacy
  (`execute`) como en la session-aware (`execute_with_ctx` / `dispatch_ctx`).
- Semantica de transaccion: las rutas autocommit y non-autocommit sin
  transaccion abierta lo ejecutan fuera de cualquier `BEGIN` implicito; si ya
  existe una transaccion activa, devuelve `TransactionAlreadyActive`.
- Wire/read-only gating: `sql_may_mutate(...)` ya trata `CHECKPOINT` como
  statement mutante para respetar las barreras de modo read-only degradado.

### Diferido explicito

- Rotacion/truncado de WAL como parte del statement SQL.
- Politicas mas finas que "cualquier transaccion activa bloquea CHECKPOINT".
- Nuevos statements administrativos estilo `FLUSH LOGS` / `CHECKPOINT FORCE`.

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --test integration_checkpoint --test integration_ddl_parser` - paso, 76 tests.
- `cargo test -p axiomdb-network --test integration_connection_lifecycle` - paso, 17 tests.
- `tools/wire-test.py` - paso, 424/424 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

### Nota de implementacion

El bug facil aqui era dejar que `CHECKPOINT` entrara por la ruta normal de
autocommit. Eso abría una transaccion implicita, la metia en `active_set` y el
statement terminaba auto-bloqueandose. El cierre correcto fue tratar
`CHECKPOINT` como una operacion administrativa fuera de transaccion implicita,
manteniendo el mismo guard compartido para el caso donde otra sesion o la
sesion actual ya tiene una transaccion abierta.

## 21.10 SQL cursors - cerrada 2026-04-21

La subfase 21.10 cierra el soporte MVP de cursores SQL de sesion:
`DECLARE`, `FETCH` y `CLOSE`. La implementacion es deliberadamente
transaction-scoped y materializada: el query se ejecuta una sola vez al
declarar el cursor, y los `FETCH` siguientes solo consumen ventanas sobre el
rowset persistido en `SessionContext`.

### Superficie SQL cerrada

```sql
BEGIN;

DECLARE c CURSOR FOR
    WITH x AS (SELECT 1 AS id)
    SELECT id FROM x
    UNION ALL
    SELECT 2;

FETCH NEXT FROM c;
FETCH 10 FROM c;
FETCH ALL FROM c;

CLOSE c;
COMMIT;
```

Alcance implementado:

- Parser/AST/analyzer: `Stmt::{DeclareCursor, FetchCursor, CloseCursor}` con
  soporte para `FETCH NEXT`, `FETCH n`, `FETCH FORWARD n`, `FETCH ALL`,
  `FROM`/`IN` y queries row-returning via `SELECT` o `SetOp`.
- Estado de sesion: `SessionContext` ahora guarda `SessionCursor { columns,
  rows, pos }` en un mapa case-insensitive, con helpers dedicados para
  declarar, buscar y cerrar cursores.
- Executor: `DECLARE` exige transaccion explicita y materializa el resultado;
  `FETCH` devuelve ventanas sobre `rows[pos..]` sin re-ejecutar el query;
  `CLOSE name` y `CLOSE ALL` limpian el estado correspondiente.
- Lifecycle: `COMMIT`, `ROLLBACK`, rollback transaccional por error,
  `COM_RESET_CONNECTION` y `COM_CHANGE_USER` cierran todos los cursores SQL.
- Wire/tests: nuevo smoke `21.10` en `tools/wire-test.py` y cobertura de reset
  / change-user en `integration_connection_lifecycle`.

### Diferido explicito

- `COM_STMT_FETCH` y cursores wire de prepared statements.
- `WITH HOLD`, `MOVE`, `PRIOR`, `ABSOLUTE`, `RELATIVE`, `BACKWARD`.
- Cursores actualizables (`FOR UPDATE`, `WHERE CURRENT OF`).
- Streaming/suspended executors en vez de rowsets materializados.

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --test integration_cursors` - paso, 6 tests.
- `cargo test -p axiomdb-network --test integration_connection_lifecycle` - paso, 17 tests.
- `cargo test -p axiomdb-sql` - paso.
- `tools/wire-test.py` - paso, 422/422 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

### Nota de implementacion

La decision clave fue no mezclar dos modelos distintos de cursor. `21.10`
cierra solo el lenguaje SQL de cursor sobre estado de sesion, mientras que
`COM_STMT_FETCH` sigue siendo un problema wire aparte. Eso mantuvo el cambio
en parser/analyzer/session/executor, evitó executor suspendido, y dejó un MVP
robusto que ya se limpia correctamente en todos los boundaries de transaccion
y conexion.

## 21.8 Expression indexes - cerrada 2026-04-21

La subfase 21.8 cierra el soporte usable de expression indexes:
`CREATE INDEX ... ((expr))` y `CREATE INDEX ... (LOWER(col))` ya persisten la
expresion canonica en catalogo, construyen y mantienen las claves evaluando la
expresion por fila, y permiten al planner reutilizar esos indexes en
predicados equivalentes.

### Superficie SQL cerrada

```sql
CREATE TABLE users (
    id INT PRIMARY KEY,
    email TEXT,
    active BOOLEAN
);

CREATE INDEX idx_lower_email ON users (LOWER(email));
CREATE INDEX idx_lower_email_active
    ON users (LOWER(email))
    WHERE active = TRUE;

EXPLAIN SELECT id
FROM users
WHERE LOWER(email) = 'alice@example.com';
```

Alcance implementado:

- Catalogo: `IndexColumnDef.expr` persiste SQL canonico por columna indexada;
  `CREATE INDEX`, `CREATE TABLE ... LIKE` y round-trips del catalogo conservan
  esa metadata.
- Build y mantenimiento: heap y clustered evalúan expresiones durante
  `CREATE INDEX`, INSERT, UPDATE y DELETE, compartiendo el compile-once usado
  tambien por partial indexes.
- Planner: equality y prefix-LIKE pueden usar expression indexes; ademas,
  los expression indexes parciales ahora se consideran cuando el `WHERE`
  completo implica el predicado guardado.
- EXPLAIN / wire: `EXPLAIN` reporta el index para casos deterministas y el
  wire smoke agrega cobertura de expression index + partial expression index.

### Diferido explicito

- Estadisticas especificas para expresiones.
- Canonicalizacion algebraica mas agresiva que igualdad SQL normalizada.
- Opclasses / metodos no-BTree para expresiones.

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-sql --test integration_expression_index` - paso, 22 tests.
- `cargo test -p axiomdb-sql` - paso.
- `tools/wire-test.py` - paso, 420/420 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

### Nota de implementacion

El cierre encontro un gap real: el matcher del planner descartaba cualquier
expression index parcial antes de evaluar implicacion del predicado. La
correccion fue mover ese filtro a `predicate_implied_by_query(...)` y buscar
el expression match dentro de arboles `AND` usando el `WHERE` completo como
contexto, en vez de tratar expression indexes y partial indexes como caminos
separados.

## 21.7 TEMP and UNLOGGED tables - cerrada 2026-04-21

La subfase 21.7 agrega soporte de primera clase para
`CREATE TEMP[TORARY] TABLE` y `CREATE UNLOGGED TABLE` sin abrir un segundo
executor ni un segundo catalogo. Las tablas TEMP se implementan como tablas
normales dentro de un schema oculto por sesion; las UNLOGGED reutilizan el
camino de escritura existente pero vacian sus filas si la base reabre en
estado sucio.

### Superficie SQL cerrada

```sql
CREATE TEMP TABLE scratch_rows (
    id INT PRIMARY KEY,
    payload TEXT
);

CREATE UNLOGGED TABLE scratch_cache (
    id INT PRIMARY KEY,
    payload TEXT
);

CREATE TEMP TABLE scratch_copy LIKE scratch_rows;
CREATE UNLOGGED TABLE cache_snapshot AS SELECT * FROM scratch_cache;
```

Alcance implementado:

- Parser/AST/catalogo: `TablePersistence` ahora viaja por `CREATE TABLE`,
  `CREATE TABLE ... LIKE`, `CREATE TABLE ... AS SELECT` y `TableDef`.
- Namespace de sesion: el primer `CREATE TEMP` aloca un schema oculto y
  antepone ese schema al `search_path`; la resolucion sin calificar sombrea a
  `public`, pero `CREATE TABLE` permanente sigue usando el schema por defecto
  no-temporal.
- Ciclo de vida TEMP: `COM_RESET_CONNECTION`, `COM_CHANGE_USER` y cierre de
  conexion limpian todas las tablas del schema temporal de la sesion.
- Recuperacion UNLOGGED: `MmapStorage` marca la base como dirty al abrir y
  clean al cerrar limpio; si la reapertura detecta dirty-open, las tablas
  `UNLOGGED` se truncan antes de servir consultas.
- Metadata: `SHOW CREATE TABLE` recompone prefijos `TEMPORARY` /
  `UNLOGGED`; `information_schema` y `SHOW` solo exponen TEMP de la sesion
  actual, mientras siguen exponiendo todas las UNLOGGED.
- Integridad: cualquier FK sobre tablas TEMP/UNLOGGED, o apuntando a ellas,
  falla explicitamente con `DbError::NotImplemented`.

### Diferido explicito

- `ON COMMIT DELETE ROWS`, `ON COMMIT DROP` y `ON COMMIT PRESERVE ROWS`.
- Sintaxis `DROP TEMPORARY TABLE`.
- Bypass de WAL en runtime para `UNLOGGED`.
- Persistencia temporal equivalente para vistas, secuencias o bases.

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-catalog` - paso.
- `cargo test -p axiomdb-storage` - paso.
- `cargo test -p axiomdb-sql --test integration_ddl_parser` - paso.
- `cargo test -p axiomdb-sql --test integration_namespacing_schema` - paso.
- `cargo test -p axiomdb-sql --test integration_g11_information_schema` - paso.
- `cargo test -p axiomdb-sql --test integration_show_full` - paso.
- `cargo test -p axiomdb-sql --test integration_g5_dml` - paso.
- `cargo test -p axiomdb-sql --test integration_temp_unlogged_tables` - paso.
- `cargo test -p axiomdb-sql` - paso.
- `cargo test -p axiomdb-network --test integration_connection_lifecycle` - paso.
- `cargo test -p axiomdb-network --test integration_open_integrity` - paso.
- `cargo test -p axiomdb-network` - paso.
- `cargo clippy -p axiomdb-sql -- -D warnings` - paso.
- `cargo clippy -p axiomdb-network -- -D warnings` - paso.
- `tools/wire-test.py` - paso, 419/419 assertions.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.

### Nota de implementacion

La decision clave fue modelar TEMP como namespace y no como storage especial:
schema oculto + `search_path` + cleanup centralizado en el lifecycle de la
conexion. Para `UNLOGGED`, el equivalente robusto fue un bit conservador de
clean shutdown en page 0 y un truncate-on-open acotado a las tablas marcadas
como `Unlogged`.

## 21.6b Exclusion constraints - cerrada 2026-04-21

La subfase 21.6b cierra el subset equality-only de exclusion constraints:
`EXCLUDE USING btree (... WITH =)`. La implementacion reutiliza el enforcement
existente de UNIQUE indexes mediante un helper index owned por la constraint,
en vez de abrir un nuevo camino row-vs-row.

### Superficie SQL cerrada

```sql
CREATE TABLE reservations (
    room_id INT,
    slot_id INT,
    EXCLUDE USING btree (room_id WITH =, slot_id WITH =)
);

ALTER TABLE reservations
    ADD CONSTRAINT reservations_room_slot_excl
    EXCLUDE USING btree (room_id WITH =, slot_id WITH =);
```

Alcance implementado:

- Parser/AST: `TableConstraint::Exclude` con elementos `column WITH operator`.
- Catalogo: `axiom_constraints` ahora persiste kind, `owned_index_id` y la
  lista de columnas / operadores de exclusion de forma backward-compatible con
  filas CHECK viejas.
- DDL: `CREATE TABLE` y `ALTER TABLE ... ADD CONSTRAINT` validan `USING btree`,
  `WITH =`, objetivos columna-only y ausencia de predicate; `CREATE TABLE`
  auto-genera nombre de constraint/helper index cuando se omite.
- Executor: INSERT/UPDATE/ODKU/ON CONFLICT/MERGE traducen duplicate errors del
  helper UNIQUE index a `ExclusionViolation { table, constraint }`.
- Metadata: `DROP CONSTRAINT` elimina el helper index owned, `DROP INDEX`
  rechaza tocar esos helpers directamente, `CREATE TABLE ... LIKE` copia la
  constraint y remapea sus helper indexes clonados, e `information_schema`
  reporta `EXCLUSION` sin filtrar el helper como UNIQUE normal.

### Diferido explicito

- `USING gist`, overlap/range operators (`&&`), range types y `WITHOUT OVERLAPS`.
- Operadores distintos de `WITH =`.
- Elementos por expresion, opclasses, collations, ordering y predicates
  `WHERE (...)`.
- DEFERRABLE / deferred enforcement.

### Validacion

- `cargo fmt --check` - paso.
- `cargo test -p axiomdb-catalog` - paso.
- `cargo test -p axiomdb-sql --test integration_ddl_parser` - paso.
- `cargo test -p axiomdb-sql --test integration_g11_information_schema` - paso.
- `cargo test -p axiomdb-sql --test integration_exclusion_constraints` - paso, 7 tests.
- `cargo test -p axiomdb-sql --test integration_errors` - paso.
- `cargo test -p axiomdb-sql --test integration_g5_dml` - paso.
- `cargo test -p axiomdb-sql` - paso.
- `cargo clippy -p axiomdb-sql -- -D warnings` - paso.
- `cargo test --workspace` - paso.
- `cargo clippy --workspace -- -D warnings` - paso.
- `tools/wire-test.py` - paso, 419/419 assertions.

### Nota de implementacion

El punto clave fue tratar exclusion constraints como metadata catalog-driven
que owns un UNIQUE index auxiliar. Eso reaprovecha validacion de filas
existentes, maintenance en heap/clustered y enforcement en todos los caminos
de escritura, mientras conserva mensajes y vistas de metadata orientados a la
constraint y no al helper interno.

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
