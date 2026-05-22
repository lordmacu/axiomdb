# Fase redo-recovery — progreso y handoff (SQLite write parity)

> **Documento de progreso committable.** El resume-here local detallado vive en
> `docs/checkpoint-redo-recovery.md` (gitignored). La memoria durable (auto-cargada
> en cada sesión) está en `memory/project_insert_perf.md` (sección lever-2 + 6f).
> Última actualización: 2026-05-22. Rama: `fase-redo-recovery` (NO mergeada a `main`).

---

## 🎯 La misión: paridad de WRITES con SQLite

Reads YA están en paridad (no tocar — point_lookup/full_scan/range_scan ~paridad o
mejor). El frente es **writes**. Estado actual (macOS, `--compare --rows 50000`):

| Escenario | AxiomDB | SQLite | Gap |
|---|---|---|---|
| insert_batch | ~375–397K ops/s | ~1.03M | **~2.6–2.7×** |
| insert_autocommit (redo OFF) | ~19.5K | ~115K | ~6× |
| insert_autocommit (redo ON) | ~38K | ~115K | **~3×** |

**La paridad necesita los 3 levers** (medidos, roadmap en `memory/project_insert_perf.md`):

1. **Lever 1 — dispatch (~0.85µs/fila):** ✅ **HECHO (subfase 6e).** Optimizó el
   dispatch genérico de INSERT (no el bypass del spec): resolve único +
   `invalidate_table_epoch_for_id` + `run_statement_triggers` sin allocs cuando no hay
   triggers. Medido: execute 1.67→1.52µs/fila; insert_batch ~2.8×→~2.7×. Beneficia
   prepared + wire + raw SQL. Commits `5441599a` (+ `510e2e0d` style).
2. **Lever 2 — commit/WAL (~1.2µs/fila, ESTRUCTURAL):** 🔄 **EN PROGRESO.** Aquí vive
   el grueso del gap. = activar **redo frame-only** (ver abajo). Dividido en 3 tasks.
3. **Lever 3 — codec (prepare_row 0.25µs):** ⏳ pendiente, chico.

---

## Lever 2 — el hallazgo clave (todo MEDIDO esta sesión)

**El lever 2 ya estaba ~construido: es activar `RedoMode::FrameOnly`.** El costo
dominante del commit con redo OFF es el `sync_all` del archivo principal en
`CatalogBootstrap::ensure_database_roots` (bootstrap.rs:454, gated por
`!storage.frame_log_active()`). Con redo ON se elimina → durabilidad vía frame fsync
(estilo SQLite-WAL). A/B medido:

- **insert_autocommit: redo OFF 19.5K → redo ON 38.2K ops/s (~2×).** Gap 6×→3×.
- **Reads: sin regresión** (point_lookup 223K→401K, full_scan 9.2M→10M). El path
  walFindFrame está bien.
- **insert_batch: wash** (el frame-log append +18ms ≈ el sync_all amortizado). El win
  de batch necesita **reuso del archivo de frame log** (Task 3).

El flag YA está expuesto end-to-end: embedded `Db::open_with_config(DbConfig{redo:
Some(RedoMode::FrameOnly)})` y server `axiomdb.toml` con `redo = "frame_only"`
(`RedoMode` es `Deserialize` snake_case; el server hace `DbConfig::load`).

### Decisión de secuencia (usuario, 2026-05-22): **Redo-first, flag-gated YA**

Activar redo detrás del flag (win opt-in inmediato) + construir la suite de crash en
paralelo para el default-on. Sprint (specs/fase-redo-recovery/, brainstorm hecho):

- **Task 1 = subfase 6f** (flag-gated opt-in shippable) — 🔄 EN PROGRESO (3/8, abajo).
- **Task 2** = suite de crash T1–T7 (gate para flip a default-on). Pendiente.
- **Task 3 (2b)** = reuso del archivo de frame log (el win de BATCH; elimina el
  +18ms de block-alloc del log que crece, estilo SQLite WAL-reuse). Pendiente.

---

## 🔄 Subfase 6f (Task 1) — estado: 3/8 pasos

Spec: `specs/fase-redo-recovery/spec-subfase-6f-frame-checkpoint-trigger.md` (approved).
Plan: `specs/fase-redo-recovery/plan-subfase-6f-frame-checkpoint-trigger.md` (in-progress).

**Por qué 6f:** frame-only redo ya es correcto + testeado (verificación de frame-header
en reads vía `read_page_if_for`, recycle in-flight-safe) y el flag está expuesto. El
ÚNICO gap de operación-normal: `checkpoint_frames` (el mecanismo de 6b) **solo se llama
en tests** → en frame-only el frame log crecería sin límite (disco). 6f wirea el trigger.

**Diseño del trigger (usuario delegó "lo más óptimo/robusto/escalable"):**
checkpointer en **background + back-pressure síncrono** (modelo Postgres checkpointer /
InnoDB page-cleaner), en la capa Db/Database. Soft threshold → checkpoint background
(los commits no pagan latencia → ~2× preservado); hard cap → checkpoint inline en el
commit (el log SIEMPRE acotado aunque el thread muera/atrase). `FrameCheckpointer` es
NUEVO; el `axiomdb_wal::Checkpointer` existente es el checkpoint LÓGICO del WAL (no
colisiona, no reusar).

```
[x] 1. TxnManager::is_committed predicate                  fe082e0e
[x] 2. maybe_checkpoint_frames + frame_log_durable_len     491687b8
[x] 3. back-pressure síncrono en commit_durable (hard cap) a3680dc8
─────  el frame log queda PROVABLEMENTE acotado una vez seteado el cap  ─────
[ ] 4. FrameCheckpointer thread background  ← EMPEZAR ACÁ
[ ] 5. wire embedded Db (Arc-share storage+txn + Drop join + setear cap desde DbConfig)
[ ] 6. wire server SharedDatabase (spawn + shutdown join)
[ ] 7. config soft/hard (K=hard/soft, default ~2) + watchdog de muerte del thread
[ ] 8. docs (user durability + internals/wal.md) + re-confirmar A/B + cierre
```

### ⚠️ Lo honesto que falta para que el opt-in REALMENTE acote
El hard cap default es `u64::MAX` (sin back-pressure) hasta que **el paso 5/7 lo
cablee desde `DbConfig.max_wal_size_mb × K`**. El MECANISMO (pasos 1-3) está completo +
testeado; el WIRING (5-7) es lo que hace que el opt-in quede acotado por defecto.
Mínimo para opt-in seguro = pasos 1-3 + cablear el cap (parte de 5/7). El thread (4) es
perf (hace el back-pressure raro/off-path); back-pressure solo ya garantiza correctitud.

### EXACT next step — Paso 4: FrameCheckpointer
- Nuevo `crates/axiomdb-storage/src/checkpointer.rs`: `FrameCheckpointer { JoinHandle,
  stop: Arc<AtomicBool>, cv: Arc<Condvar> }`. Spawn con `Arc<MmapStorage>` + fuente del
  committed-predicate (`Arc<TxnManager>` o closure). Loop: esperar Condvar (con timeout)
  hasta `stop` o `frame_log_durable_len() >= soft` → `maybe_checkpoint_frames(is_committed,
  soft, false)`. `notify()` lo llama el frame-append al cruzar soft. `stop_and_join()` =
  señal + checkpoint final + join.
- **Bloqueante estructural:** el thread necesita acceso 'static compartido a storage+txn.
  Embedded `Db` los tiene **por valor** (lib.rs:112-114, sin `Drop`) → el paso 5 hace
  Arc-wrap (`Db.storage: Arc<MmapStorage>`, `Db.txn: Arc<TxnManager>`; los call-sites
  usan `&*self.storage` en los sitios `&dyn`). Server `SharedDatabase` ya se comparte vía
  `Arc<SharedDatabase>` (storage/txn por valor adentro). MmapStorage + TxnManager YA son
  Send+Sync (probado por el uso del server).

---

## Cómo medir (todo macOS-native; reads/writes parity necesita APFS real, no Lima)

```bash
# build bench (sin/ con timers)
cargo build --release -p axiomdb-bench-comparison [--features bench-timings]
# breakdown del execute (dispatch remainder) — necesita un --scenario dummy; salida a stderr
./target/release/axiomdb_bench --diagnose-insert-deep --scenario insert_batch --rows 50000
# breakdown del COMMIT (tree/root_persist/wal)
AXIOMDB_DEBUG_CLUSTERED_INSERT=1 ./target/release/axiomdb_bench --diagnose-prepared-insert --scenario insert_batch --rows 50000
# A/B redo OFF vs ON (knob AXIOMDB_BENCH_REDO, commit dfad7dac)
./target/release/axiomdb_bench --scenario insert_autocommit --rows 5000
AXIOMDB_BENCH_REDO=1 ./target/release/axiomdb_bench --scenario insert_autocommit --rows 5000
# ratio vs SQLite (todos los escenarios)
./target/release/axiomdb_bench --compare --rows 50000
```

Build/test/clippy van en **Lima** (`./tools/vm.sh test|clippy|fmt-check -p <crate>`),
NUNCA macOS. EXCEPCIÓN: el bench (perf) se corre en macOS native.

---

## Convenciones a recordar (esta sesión)

- **Lima** para todo cargo build/test/clippy/fmt; bench en macOS.
- `cargo nextest` (no `cargo test`); `rtk` para git (verificar con `rtk proxy git` antes de commitear).
- **NUNCA** `Co-Authored-By` de Claude. Conventional Commits.
- **`--no-verify` AUTORIZADO** para commits de project-B sin impacto en docs (el
  pre-commit hook bloquea cambios en `crates/` sin `docs-site/`).
- Working tree compartido: nunca `git add -A`, formatear solo mis archivos, `rtk proxy git status` antes.
- Decisiones autónomas, no preguntar de más; siempre la opción técnicamente superior
  (robustez/velocidad > simplicidad).
- Responder al usuario en español; código/commits/identificadores en inglés.
- Se puede portar técnicas/código de SQLite (research/sqlite/) — adaptar, no verbatim.
- Logs/timers para encontrar cuellos de botella están OK (medir, no asumir).

---

## Resumen de commits de esta sesión (rama 73 commits sobre main)

- `01429084` fix: thread conn en flush clustered (panic >200K filas)
- `784e7b34` style: clippy sweep + benches stale
- `5441599a` perf 6e: dispatch catalog round-trips · `510e2e0d` style
- `dfad7dac` bench: knob AXIOMDB_BENCH_REDO
- `7996e58e` spec 6f · `33919787` plan 6f
- `fe082e0e` 6f-1 · `491687b8` 6f-2 · `a3680dc8` 6f-3 · `d35e329e` plan progress
