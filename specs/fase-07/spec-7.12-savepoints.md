# Spec: 7.12 — Basic SQL Savepoints

## What to build

SQL savepoint support: `SAVEPOINT name`, `ROLLBACK TO [SAVEPOINT] name`,
`RELEASE [SAVEPOINT] name`. ORMs (Django, Rails, Sequelize, SQLAlchemy) use
savepoints internally for partial error recovery in long transactions.

## Behavior (MySQL-compatible)

| Command | Effect |
|---------|--------|
| `SAVEPOINT sp1` | Capture current undo position as named savepoint; push onto stack |
| `ROLLBACK TO sp1` | Undo all changes since sp1; destroy all savepoints created after sp1; sp1 remains active |
| `RELEASE sp1` | Destroy sp1 and all savepoints created after it; changes stay committed within the transaction |
| `COMMIT` | All savepoints destroyed; transaction committed |
| `ROLLBACK` | All savepoints destroyed; transaction rolled back |

### Edge cases
- **Duplicate names:** allowed (most recent wins on lookup) — MySQL behavior
- **ROLLBACK TO released savepoint:** error "SAVEPOINT does not exist"
- **SAVEPOINT outside transaction:** error (MySQL requires explicit BEGIN)
- **Nested:** unlimited depth

## Inputs / Outputs

- `SAVEPOINT sp1` → `QueryResult::Empty`
- `ROLLBACK TO sp1` → `QueryResult::Empty` (or error if not found)
- `RELEASE sp1` → `QueryResult::Empty` (or error if not found)

## Acceptance Criteria

- [ ] `SAVEPOINT name` parsed and creates named savepoint
- [ ] `ROLLBACK TO name` rolls back to savepoint, destroys later savepoints
- [ ] `ROLLBACK TO SAVEPOINT name` (MySQL syntax with SAVEPOINT keyword) also works
- [ ] `RELEASE name` destroys savepoint and later ones
- [ ] `RELEASE SAVEPOINT name` also works
- [ ] Duplicate names: most recent wins
- [ ] ROLLBACK TO non-existent savepoint → error
- [ ] RELEASE non-existent savepoint → error
- [ ] COMMIT/ROLLBACK clears all savepoints
- [ ] SAVEPOINT outside explicit transaction → error
- [ ] Index undo (UndoIndexInsert) works correctly with savepoint rollback
- [ ] Tests pass

## Out of Scope
- Transaction savepoints (SQLite auto-BEGIN on first SAVEPOINT)
- Deferred constraint checking per savepoint
- Savepoint-level lock tracking

## Dependencies
- `TxnManager::savepoint()` — already exists
- `TxnManager::rollback_to_savepoint(sp, storage)` — already exists
- `UndoOp::UndoIndexInsert` handling in rollback_to_savepoint — already exists (7.3b)
