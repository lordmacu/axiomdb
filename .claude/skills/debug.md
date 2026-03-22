# /debug — Debugging sistemático

Nunca hacer cambios aleatorios esperando que funcione. Seguir este proceso.

## Paso 1 — Reproducir con test mínimo

Antes de investigar, escribir el test más pequeño posible que demuestre el bug:

```rust
#[test]
fn test_bug_reproduccion() {
    // Setup mínimo — solo lo necesario para reproducir
    let storage = MemoryStorage::new();

    // Acción que causa el bug
    let result = do_thing(storage);

    // Verificar el comportamiento incorrecto
    assert_eq!(result, expected); // esto debe FALLAR para confirmar el bug
}
```

Si no puedes escribir un test que reproduzca el bug, el bug no está bien definido.
Volver al usuario y pedir más información.

## Paso 2 — Formular hipótesis (mínimo 2)

```
Hipótesis A: [qué crees que está mal]
  Evidencia a favor: [qué observaciones apoyan esto]
  Cómo verificar: [experimento concreto]

Hipótesis B: [otra causa posible]
  Evidencia a favor: [qué observaciones apoyan esto]
  Cómo verificar: [experimento concreto]
```

No asumir la primera hipótesis. Siempre considerar al menos una alternativa.

## Paso 3 — Verificar cada hipótesis

```rust
// Agregar logging temporal para verificar (remover después)
tracing::debug!("valor en punto X: {:?}", valor);

// O usar dbg! para valores rápidos
dbg!(&estructura);

// Para concurrencia: agregar contadores atómicos
static COUNTER: AtomicU64 = AtomicU64::new(0);
COUNTER.fetch_add(1, Ordering::Relaxed);
```

Verificar hipótesis en orden, no en paralelo.
Una vez descartada una hipótesis, documentar por qué.

## Paso 4 — Fix en el lugar correcto

```
❌ Parchar el síntoma:
   if result.is_err() { return Ok(default_value); }

✅ Fix la causa raíz:
   // El problema era que X no inicializaba Y correctamente
   // Fix: inicializar Y antes de usar X
```

El fix debe ser el mínimo cambio que resuelve la causa raíz.
No aprovechar para "limpiar" código no relacionado (eso va en otro commit).

## Paso 5 — Test de regresión

```rust
// El test de reproducción del paso 1 ahora debe PASAR
// Renombrarlo para que documente el bug que previene:

#[test]
fn test_btree_no_pierde_keys_despues_de_split() {
    // Este test previene la regresión del bug donde el split
    // del nodo interno perdía la primera key del hijo derecho
    ...
}
```

Agregar el test a la suite permanente. Nunca borrarlo.

## Herramientas de debugging en Rust

```bash
# Ver backtrace completo
RUST_BACKTRACE=full cargo test nombre_del_test

# Sanitizers (detectar UB, memory leaks)
RUSTFLAGS="-Z sanitizer=address" cargo +nightly test
RUSTFLAGS="-Z sanitizer=thread" cargo +nightly test  # race conditions

# Miri (detectar UB en código unsafe)
cargo +nightly miri test nombre_del_test

# Para async: tokio-console
cargo add tokio-console
# En código: console_subscriber::init();
```
