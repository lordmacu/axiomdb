# /brainstorm — Explorar antes de proponer

Antes de proponer ninguna solución, ejecutar este protocolo completo:

## Paso 1 — Leer contexto
- Leer `db.md` sección de la fase actual
- Leer `docs/fase-anterior.md` si existe
- Leer archivos del codebase relevantes a la tarea
- Leer `specs/fase-actual/` si existe

## Paso 2 — Hacer preguntas al usuario
No proponer nada hasta tener respuestas a:
- ¿Cuál es el comportamiento esperado exacto?
- ¿Hay casos borde conocidos que debo manejar?
- ¿Hay restricciones de rendimiento (latencia, throughput)?
- ¿Hay restricciones de compatibilidad con fases anteriores?
- ¿Cuánto tiempo aproximado tenemos para esta fase?

## Paso 3 — Proponer enfoques con trade-offs
Siempre presentar **al menos 2 opciones**, nunca solo la "mejor":

```
Enfoque A: [nombre]
  Ventajas: [lista]
  Desventajas: [lista]
  Cuándo elegirlo: [condición]

Enfoque B: [nombre]
  Ventajas: [lista]
  Desventajas: [lista]
  Cuándo elegirlo: [condición]
```

## Paso 4 — Sprint con dependencias
Si la tarea tiene subtareas, escribir sprint explícito:

```
Sprint: [nombre de la fase]
Estimación: [N horas/días]

├── Tarea 1: [nombre]
│   Descripción: [qué hace]
│   Dependencias: ninguna
│   Criterio de done: [verificable]
│
├── Tarea 2: [nombre]
│   Descripción: [qué hace]
│   Dependencias: Tarea 1
│   Criterio de done: [verificable]
│
└── Tarea 3: [nombre]
    Descripción: [qué hace]
    Dependencias: Tarea 1
    Criterio de done: [verificable]
```

## Output esperado
Al final del brainstorm, el usuario y Claude deben tener acordado:
- [ ] Enfoque elegido y por qué
- [ ] Sprint con tareas y dependencias
- [ ] Criterios de done claros
- [ ] Riesgos identificados

Siguiente paso: `/spec-task` para la primera tarea del sprint.
