# /fase-completa — Protocolo de cierre de fase

Ejecutar este protocolo COMPLETO al terminar cada fase. Sin excepciones.

## Paso 1 — Verificar calidad del código

```bash
# Tests
cargo test --workspace
# Si falla: NO continuar hasta que pasen

# Linting
cargo clippy --workspace -- -D warnings
# Si hay warnings: NO continuar hasta resolverlos

# Formato
cargo fmt --check
# Si hay diferencias: correr cargo fmt y agregar al commit

# Documentación de unsafe
grep -r "unsafe" crates/ --include="*.rs" | grep -v "// SAFETY:"
# Si hay bloques unsafe sin SAFETY: agregar el comentario
```

## Paso 2 — Benchmarks (si aplica)

```bash
cargo bench --workspace 2>&1 | tail -30
# Comparar contra presupuesto en CLAUDE.md
# Si alguna operación crítica regresó >5%: investigar antes de continuar
```

## Paso 3 — Documentar la fase

Crear `docs/fase-N.md` con este template:

```markdown
# Fase N — [Nombre]

Completada: [fecha]
Semanas estimadas: N-M | Semanas reales: X

## Qué se construyó
[descripción de lo implementado]

## Crates creados/modificados
- `crates/dbyo-X` — [qué hace]

## Decisiones tomadas
- [decisión] → [razón]

## Tests escritos
- [test] — [qué verifica]

## Cómo continuar desde aquí
[instrucción exacta para la próxima sesión]

## Próxima fase
Fase N+1 — [nombre]. Ver `specs/fase-N+1/` y `db.md`.
```

## Paso 4 — Actualizar memoria

```
memory/project_state.md:
  - Mover fase N de "pendientes" a "completadas"
  - Actualizar "Fase actual" a N+1

memory/architecture.md:
  - Agregar los crates y structs realmente implementados
  - Actualizar el árbol de directorios

memory/decisions.md:
  - Agregar decisiones técnicas tomadas durante la fase

memory/lessons.md (si hubo aprendizajes):
  - ### [Fase N] Título del aprendizaje
  - Problema, causa, solución, cuándo aplicar
```

## Paso 5 — Commit

```bash
git add -A
git commit -m "feat(fase-N): [descripción concisa]

- [detalle 1]
- [detalle 2]

Fase N/34 completada. Ver docs/fase-N.md
Spec: specs/fase-N/ | Tests: X pasando

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

## Paso 6 — Confirmar al usuario

Reportar:
```
✅ Fase N completada

Tests:      X/X pasando
Clippy:     0 warnings
Benchmarks: [dentro/fuera] del presupuesto

Documentado en: docs/fase-N.md
Memoria:        actualizada
Commit:         [hash]

Próxima fase: N+1 — [nombre]
Para continuar: /brainstorm sobre la Fase N+1
```
