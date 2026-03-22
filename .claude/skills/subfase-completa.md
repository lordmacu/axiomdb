# /subfase-completa — Marcar subfase como completada

Ejecutar este skill al terminar cada subfase individual.
Actualiza el progreso, la memoria y hace commit automático.

## Uso

```
/subfase-completa N.M
```

Ejemplo: `/subfase-completa 1.3` marca la subfase 1.3 como completada.

## Proceso que ejecuta este skill

### Paso 1 — Verificar que la subfase realmente está lista

```bash
cd /Users/cristian/dbyo

# Tests del crate afectado
cargo test -p dbyo-CRATE --quiet 2>&1 | tail -5

# Clippy sin warnings
cargo clippy -p dbyo-CRATE -- -D warnings 2>&1 | head -10

# Formato correcto
cargo fmt --check 2>&1 | head -5
```

Si alguno falla: **NO marcar como completada.** Resolver primero.

### Paso 2 — Marcar en docs/progreso.md

Buscar la línea de la subfase N.M y cambiar:
```
- [ ] N.M ⏳ descripción
```
por:
```
- [x] N.M ✅ descripción — completada YYYY-MM-DD
```

También actualizar el estado de la fase padre si todas sus subfases están completas:
```
### Fase N — Nombre `⏳`   →   ### Fase N — Nombre `✅`
```

### Paso 3 — Actualizar estadísticas al final del archivo

Recalcular y actualizar el bloque de estadísticas:
```
Total subfases:  185
Completadas:      X  (Y%)
En progreso:      Z  (W%)
Pendientes:     ...

Fase actual:     [siguiente subfase pendiente]
Última completada: N.M — descripción — YYYY-MM-DD
```

### Paso 4 — Actualizar memory/project_state.md

```markdown
## En progreso
- Subfase [N.M+1]: [descripción]

## Completadas recientemente
- Subfase N.M: descripción (YYYY-MM-DD)
```

### Paso 5 — Si la fase completa terminó, actualizar memory/architecture.md

Solo si N.M fue la última subfase de la Fase N:
```markdown
## Crates implementados
- `dbyo-NOMBRE` — descripción (Fase N completada YYYY-MM-DD)
```

### Paso 6 — Commit

```bash
git add docs/progreso.md .claude/projects/*/memory/project_state.md
git commit -m "progress(N.M): completar [descripción breve de la subfase]

Subfase N.M de Fase N completada.
Progreso: X/185 subfases (Y%)

Co-Authored-By: Claude Sonnet 4.6 <noreply@anthropic.com>"
```

### Paso 7 — Reportar al usuario

```
✅ Subfase N.M completada — [descripción]

Progreso fase N: [X de Y subfases] ████░░░░ 60%
Progreso total:  [X de 185]        ██░░░░░░  8%

Siguiente subfase: N.M+1 — [descripción]
```

---

## Formato de referencia rápida

```
⏳ pendiente
🔄 en progreso (marcar manualmente si empiezas sin terminar)
✅ completada
⏸ bloqueada (depende de algo externo)
```

Para marcar en progreso sin completar:
```
- [ ] N.M 🔄 descripción — iniciada YYYY-MM-DD
```

Para marcar como bloqueada:
```
- [ ] N.M ⏸ descripción — bloqueada por: [razón]
```
