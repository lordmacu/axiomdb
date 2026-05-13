---
name: Lima VM (axiomdb) — gestión bajo demanda
description: La VM de Lima llamada axiomdb es requerida para este proyecto. Se debe iniciar antes de trabajar y detener al terminar para liberar recursos.
type: project
---

La VM de Lima `axiomdb` es necesaria para el proyecto nexusdb (base de datos Rust). Usa 10 GB de RAM cuando está corriendo.

**Why:** Lima consume 10GB de RAM que compiten con Ollama (Gemma 4 ~5GB) y otras herramientas. Solo debe correr cuando se trabaja activamente en nexusdb.

**How to apply:**
- Antes de cualquier tarea en este proyecto: verificar si Lima está corriendo con `limactl list`
- Si está parada (`Stopped`): iniciarla con `limactl start axiomdb` antes de proceder
- Al terminar la sesión de trabajo: recordar al usuario detenerla con `limactl stop axiomdb`
- Si el usuario dice "para Lima" o "detén la VM": ejecutar `limactl stop axiomdb`
- Si el usuario dice "activa Lima" o "inicia la VM": ejecutar `limactl start axiomdb`

**Comandos clave:**
- Estado: `limactl list`
- Iniciar: `limactl start axiomdb`
- Detener: `limactl stop axiomdb`
- Entrar a la VM: `limactl shell axiomdb`
