# /checkpoint — Guardar contexto al pausar

Cuando hay que pausar en medio de una tarea, guardar el estado exacto
para que la próxima sesión pueda continuar sin preguntas.

## Crear checkpoint

```bash
FECHA=$(date +%Y%m%d-%H%M)
cat > /Users/cristian/dbyo/docs/checkpoint-$FECHA.md << 'CHECKPOINT'
# Checkpoint [FECHA]

## Estado de la fase
Fase: N — [nombre]
Tarea actual: [nombre de la tarea dentro del sprint]

## Qué se estaba haciendo exactamente
[descripción precisa — qué función, qué archivo, qué problema]

## Decisión pendiente (si hay)
[qué había que decidir antes de continuar]
[opciones consideradas]
[información faltante para decidir]

## Próximo paso exacto
[instrucción precisa — suficiente para continuar sin contexto]
Ejemplo: "Implementar fn insert() en crates/dbyo-index/src/btree.rs línea 42,
         siguiendo el algoritmo del spec en specs/fase-02/spec-btree.md sección 'Insert'"

## Estado de los tests
cargo test resultado: [X pasando, Y fallando]
Tests que fallan (esperado durante desarrollo):
- [nombre del test] — [por qué falla, qué falta]

## Archivos modificados
- [archivo] — [qué cambió]
- [archivo] — [qué cambió]

## Lo que NO hay que volver a hacer
[cosas ya intentadas que no funcionaron]
CHECKPOINT

git add docs/checkpoint-$FECHA.md
git commit -m "checkpoint: pausar en [descripción breve]"
```

## Al iniciar una sesión con checkpoint

```
1. Leer docs/checkpoint-FECHA.md más reciente
2. Correr cargo test --workspace para ver el estado actual
3. Leer el "Próximo paso exacto" y ejecutarlo
4. Borrar el checkpoint cuando la tarea se complete
```

## Cuándo crear checkpoint

- Al final de la sesión sin haber completado la fase
- Antes de una pausa larga (> 2 horas)
- Cuando el contexto de la conversación está casi lleno
- Cuando hay una decisión pendiente que requiere input externo
