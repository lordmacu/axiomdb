# NexusDB — Motor de Base de Datos en Rust

> Proyecto universitario: NexusDB es un motor de BD portable, rápido, con SQL, índices, llaves foráneas y concurrencia.

> Inspirado en ISAM pero moderno. Objetivo: superar a MySQL en benchmarks específicos.

---

## Resumen de decisiones

| Decisión | Elección | Razón |
|---|---|---|
| Lenguaje del motor | **Rust** | Sin GC, control de memoria, máxima velocidad |
| Modo web | **MySQL wire protocol** | PHP/Python conectan sin drivers custom |
| Modo desktop | **Embedded library + C FFI** | In-process como SQLite, cero latencia de red |
| Storage | **mmap + páginas 8KB** | Elimina double-buffering de InnoDB |
| Índice | **Copy-on-Write B+ Tree** | Readers sin locks = alta concurrencia |
| Durabilidad | **WAL append-only** | Sin double-write buffer |
| Concurrencia | **Tokio async** | Miles de conexiones sin overhead de threads |
| Queries | **Vectorized + SIMD** | 10-50x en scans vs row-by-row |

---

## Modos de uso

### Modo Servidor — Aplicaciones Web

```
Cliente Web (PHP/Python/Node)
        │
        │ TCP :3306 (MySQL wire protocol)
        ▼
  Motor Rust corriendo como daemon
        │
        ▼
  archivo.db  /  archivo.wal
```

- Un proceso servidor independiente
- Múltiples clientes conectados simultáneamente
- Ideal para: APIs REST, backend web, microservicios

### Modo Embebido — Aplicaciones de Escritorio

```
Aplicación (C++/Python/Java/Electron)
        │
        │ llamada de función directa (C FFI / binding nativo)
        ▼
  Motor Rust in-process (como SQLite)
        │
        ▼
  archivo.db  /  archivo.wal  (local)
```

- El motor vive dentro del proceso de la app
- Cero latencia de red (llamadas directas en memoria)
- Sin daemon, sin puerto, sin instalación extra
- Ideal para: apps Electron, desktop C++/Qt, CLI tools, Python scripts

### Comparativa de modos

| Característica | Modo Servidor | Modo Embebido |
|---|---|---|
| Latencia | ~0.1ms (TCP loopback) | ~1µs (in-process) |
| Múltiples procesos | Sí | No (un proceso a la vez) |
| Instalación | Daemon + puerto | Solo la librería (.so/.dll) |
| Ideal para | Web, APIs, microservicios | Desktop, CLI, scripts |

---

## Clientes compatibles (sin drivers custom)

```
PHP    → PDO::mysql o mysqli        (apuntan a :3306)
Python → PyMySQL o mysql-connector  (apuntan a :3306)
GUI    → MySQL Workbench, DBeaver   (igual)
```

El servidor implementa el **MySQL wire protocol**, por lo que cualquier cliente MySQL
existente se conecta directamente sin modificaciones.

---

## Arquitectura general

```
┌──────────────────────────────────────────────────────────────────┐
│                         CLIENTES                                 │
│                                                                  │
│  WEB: PHP (PDO::mysql)   Python (PyMySQL)   Node.js             │
│  DESKTOP: C++ / Qt       Electron (JS)      Python script        │
└──────────┬───────────────────────────────────────┬──────────────┘
           │ TCP :3306                             │ C FFI / binding
           │ MySQL wire protocol                   │ in-process
           ▼                                       ▼
┌─────────────────────┐               ┌────────────────────────┐
│   MODO SERVIDOR     │               │    MODO EMBEBIDO       │
│   (web / APIs)      │               │    (desktop / CLI)     │
│                     │               │                        │
│  Tokio TCP listener │               │  lib.rs expone API     │
│  MySQL handshake    │               │  C FFI (#[no_mangle])  │
│  Auth básica        │               │  Sin red, sin daemon   │
└──────────┬──────────┘               └───────────┬────────────┘
           │                                      │
           └──────────────┬───────────────────────┘
                          │ (mismo motor, distinto entry point)
           ┌──────────────▼──────────────────────────────────┐
           │               MOTOR RUST                        │
           │                                                 │
           │  SQL Parser  →  Query Planner  →  Executor      │
           │  (nom crate)    (cost-based)    (vectorized      │
           │                                 + morsel         │
           │                                 + fusion         │
           │                                 + late-mat.)     │
           └──────────────┬──────────────────────────────────┘
                          │
           ┌──────────────▼──────────────────────────────────┐
           │              STORAGE ENGINE                     │
           │                                                 │
           │  Copy-on-Write B+ Tree  (MVCC sin locks)        │
           │  mmap del archivo .db   (zero double-buffer)    │
           │  WAL append-only        (sin double-write)      │
           │  FK Checker             (índice inverso)        │
           └─────────────────────────────────────────────────┘
                          │
                 ┌────────▼────────┐
                 │  tabla.db       │  ← páginas mmap
                 │  tabla.wal      │  ← log secuencial
                 │  tabla.idx      │  ← B+ Tree por índice
                 └─────────────────┘
```

---

## Modo Embebido — API para Desktop

### Estructura del crate

```
dbyo/
├── Cargo.toml
├── src/
│   ├── lib.rs          ← API pública Rust + C FFI
│   ├── server.rs       ← entry point modo servidor (TCP + MySQL protocol)
│   ├── engine/         ← motor compartido (storage, parser, executor)
│   └── ffi.rs          ← bindings C exportados
```

### API Rust nativa (para apps Rust/Electron vía Neon)

```rust
// src/lib.rs
pub struct Database {
    engine: Arc<Engine>,
}

impl Database {
    /// Abrir o crear una BD en disco
    pub fn open(path: &str) -> Result<Self, DbError> { ... }

    /// Abrir BD en memoria (tests, CLI temporal)
    pub fn open_in_memory() -> Result<Self, DbError> { ... }

    /// Ejecutar SQL y obtener resultado
    pub fn execute(&self, sql: &str) -> Result<QueryResult, DbError> { ... }

    /// Transacción explícita
    pub fn transaction<F>(&self, f: F) -> Result<(), DbError>
    where F: FnOnce(&Transaction) -> Result<(), DbError> { ... }
}
```

### C FFI (para C++, Qt, Java JNI, Python ctypes)

```rust
// src/ffi.rs
use std::ffi::{CStr, CString};

#[no_mangle]
pub extern "C" fn dbyo_open(path: *const c_char) -> *mut Database {
    let path = unsafe { CStr::from_ptr(path) }.to_str().unwrap();
    match Database::open(path) {
        Ok(db) => Box::into_raw(Box::new(db)),
        Err(_)  => std::ptr::null_mut(),
    }
}

#[no_mangle]
pub extern "C" fn dbyo_execute(
    db: *mut Database,
    sql: *const c_char,
    out_json: *mut *mut c_char,  // resultado como JSON
) -> c_int {
    // retorna 0 = OK, código de error si falla
    ...
}

#[no_mangle]
pub extern "C" fn dbyo_close(db: *mut Database) {
    if !db.is_null() {
        unsafe { drop(Box::from_raw(db)) };
    }
}
```

### Cargo.toml — compilar como librería Y como binario

```toml
[lib]
name = "dbyo"
crate-type = ["cdylib", "rlib"]
# cdylib → .so (Linux) / .dll (Windows) / .dylib (macOS) para C FFI
# rlib   → librería Rust para apps Rust nativas

[[bin]]
name = "dbyo-server"
path = "src/server.rs"
# El servidor TCP corre como binario independiente
```

### Uso desde distintos lenguajes (modo embebido)

**Python (ctypes):**
```python
import ctypes
db = ctypes.CDLL("./libdbyo.so")
conn = db.dbyo_open(b"./myapp.db")
db.dbyo_execute(conn, b"SELECT * FROM users WHERE id = 1", ...)
db.dbyo_close(conn)
```

**C++ / Qt:**
```cpp
#include "dbyo.h"
auto* db = dbyo_open("./myapp.db");
dbyo_execute(db, "INSERT INTO logs VALUES (1, 'inicio')", &result);
dbyo_close(db);
```

**Electron / Node.js (via Neon):**
```js
const { Database } = require('./dbyo-node');
const db = new Database('./myapp.db');
const rows = db.execute('SELECT * FROM products LIMIT 10');
```

---

## Por qué MySQL es lento (y cómo lo atacamos)

### Problema 1: Double-Buffering

```
MySQL InnoDB:
  Disco → OS Page Cache → Buffer Pool de InnoDB → Query
            (copia 1)          (copia 2)

  Mismos datos DOS VECES en RAM.
  Buffer Pool puede ser 80% del servidor y sigue duplicando.

Nuestra BD (mmap):
  Disco → OS Page Cache → Query
            (copia 1, directa)

  El OS ya cachea. No reinventamos el buffer pool.
```

### Problema 2: Double-Write Buffer de InnoDB

```
MySQL escribe cada página DOS VECES:
  Write 1 → doublewrite buffer (área especial del disco)
  Write 2 → posición real en .ibd

Con WAL + checksum por página esto es innecesario.
Nosotros lo eliminamos completamente.
```

### Problema 3: MVCC con Undo Log separado

```
MySQL:
  Versión nueva → B+ Tree
  Versión vieja → undo log (ibdata1) → I/O adicional para reads históricos

Nuestra BD (Copy-on-Write B+ Tree):
  Readers tienen puntero al root viejo
  Las versiones viejas son páginas del árbol mismo
  Zero I/O adicional para lecturas concurrentes
```

### Problema 4: Thread-per-connection

```
MySQL:
  1000 conexiones = 1000 threads del OS
  Cada thread = ~8MB stack = 8GB solo en stacks
  Context switching masivo

Nuestra BD (Tokio async):
  1000 conexiones = ~8 threads reales del OS
  Multiplexados con async/await
  Sin context switch innecesario
```

### Problema 5: Ejecución row-by-row sin SIMD

```
MySQL evalúa WHERE fila por fila:
  for row in table:
    if row.age > 25: emit(row)     ← escalar, 1 a la vez

Nuestra BD (vectorized + SIMD AVX2):
  cargar 256 valores de age en registros AVX2
  comparar 256 valores con UNA instrucción
  máscara de bits → emit en batch

  ~16x más rápido en table scans
```

---

## Sistema de Tipos

### Tabla completa de tipos soportados

| Tipo SQL | Alias | Bytes | Rust interno | Notas |
|---|---|---|---|---|
| `BOOL` | `BOOLEAN` | 1 | `bool` | |
| `TINYINT` | `INT1` | 1 | `i8` | -128 a 127 |
| `UTINYINT` | `UINT1` | 1 | `u8` | 0 a 255 |
| `SMALLINT` | `INT2` | 2 | `i16` | |
| `USMALLINT` | `UINT2` | 2 | `u16` | |
| `INT` | `INTEGER, INT4` | 4 | `i32` | |
| `UINT` | `UINT4` | 4 | `u32` | |
| `BIGINT` | `INT8` | 8 | `i64` | |
| `UBIGINT` | `UINT8` | 8 | `u64` | LSN, page_id internos |
| `HUGEINT` | `INT16` | 16 | `i128` | criptografía, checksums |
| `REAL` | `FLOAT4, FLOAT` | 4 | `f32` | coordenadas, ratings |
| `DOUBLE` | `FLOAT8, DOUBLE PRECISION` | 8 | `f64` | cálculos científicos |
| `DECIMAL(p,s)` | `NUMERIC(p,s)` | 16 | `rust_decimal::Decimal` | dinero, contabilidad |
| `CHAR(n)` | | n | `[u8; n]` | longitud fija |
| `VARCHAR(n)` | | ≤n | `CompactStr` | máximo n chars |
| `TEXT` | | var | `CompactStr` / TOAST | ilimitado |
| `CITEXT` | | var | `CompactStr` | case-insensitive |
| `BYTEA` | `BLOB, BYTES` | var | `Arc<[u8]>` | binario, TOAST si >2KB |
| `BIT(n)` | | n bits | `BitVec` | exactamente n bits |
| `VARBIT(n)` | | ≤n bits | `BitVec` | hasta n bits |
| `DATE` | | 4 | `time::Date` | solo fecha |
| `TIME` | | 8 | `time::Time` | solo hora |
| `TIMETZ` | | 12 | `time::Time + offset` | hora con zona |
| `TIMESTAMP` | | 8 | `i64` µs | sin zona (local) |
| `TIMESTAMPTZ` | | 8 | `i64` µs UTC | **recomendado** |
| `INTERVAL` | | 16 | `DbInterval` | meses + días + µs |
| `UUID` | | 16 | `[u8; 16]` | v4 aleatorio, v7 ordenable |
| `INET` | | 16 | `std::net::IpAddr` | IPv4 e IPv6 |
| `CIDR` | | 17 | `DbCidr` | red IP con prefijo |
| `MACADDR` | | 6 | `[u8; 6]` | dirección MAC |
| `JSON` | `JSONB` | var | `Arc<serde_json::Value>` | TOAST si >2KB |
| `VECTOR(n)` | | 4n | `Arc<Vec<f32>>` | embeddings IA |
| `T[]` | | var | `Arc<Vec<Value>>` | array de cualquier tipo |
| `ENUM` | | 1-4 | `u32` (índice) | validado en insert |
| `RANGE(T)` | | var | `Range<T>` | int4range, daterange, tsrange |
| `COMPOSITE` | | var | `Vec<Value>` | CREATE TYPE … AS (…) |
| `DOMAIN` | | = base | = base | CHECK sobre tipo base |

---

### Value enum interno — representación compacta (24 bytes)

```rust
use compact_str::CompactStr;  // strings ≤23 chars en stack, sin heap
use std::sync::Arc;

#[repr(u8)]
enum Value {
    // Nulos y booleanos
    Null,
    Bool(bool),                         //  1 byte

    // Enteros con signo
    TinyInt(i8),                        //  1 byte
    SmallInt(i16),                      //  2 bytes
    Int(i32),                           //  4 bytes
    BigInt(i64),                        //  8 bytes
    HugeInt(i128),                      // 16 bytes

    // Enteros sin signo
    UTinyInt(u8),
    USmallInt(u16),
    UInt(u32),
    UBigInt(u64),

    // Punto flotante
    Float(f32),                         //  4 bytes
    Double(f64),                        //  8 bytes
    Decimal(rust_decimal::Decimal),     // 16 bytes — exacto

    // Texto
    Text(CompactStr),                   // 24 bytes stack si ≤23 chars
    Bytes(Arc<[u8]>),                   // zero-copy con Arc

    // Fecha/hora
    Date(i32),                          //  4 bytes — días desde epoch
    Time(i64),                          //  8 bytes — µs desde medianoche
    Timestamp(i64),                     //  8 bytes — µs desde epoch UTC
    Interval(i32, i32, i64),            // 16 bytes — meses, días, µs

    // Tipos especiales
    Uuid([u8; 16]),                     // 16 bytes — NO como String
    Inet(std::net::IpAddr),             // 16 bytes — IPv4 e IPv6
    MacAddr([u8; 6]),                   //  6 bytes

    // Colecciones (compartidas con Arc para zero-copy)
    Array(Arc<Vec<Value>>),
    Json(Arc<serde_json::Value>),
    Vector(Arc<Vec<f32>>),              // embeddings IA

    // Rango
    Range(Arc<(Option<Value>, Option<Value>, bool, bool)>),
}
// Tamaño total: 24 bytes gracias a CompactStr + Arc para valores grandes
```

---

### NULL bitmap — sin Option<T> por valor

```rust
// MAL: Option<T> añade 8 bytes por cada campo nullable
struct RowNaive {
    id:    Option<i32>,   // 8 bytes
    age:   Option<i32>,   // 8 bytes
    score: Option<f64>,   // 16 bytes
}  // total: 32 bytes para 3 valores simples

// BIEN: 1 bit por columna en el header de la fila
#[repr(C)]
struct Row {
    null_bitmap: u64,      // bit i = 1 → columna i es NULL
    data:        [u8; N],  // valores empaquetados sin Option
}

fn is_null(row: &Row, col: usize) -> bool {
    row.null_bitmap & (1 << col) != 0
}

fn set_null(row: &mut Row, col: usize) {
    row.null_bitmap |= 1 << col;
}
// Ahorro: 7 bytes × columnas_nullable por fila
```

---

### Column Encoding — compresión transparente

```rust
enum ColumnEncoding {
    /// Sin encoding — para alta cardinalidad (UUIDs, hashes)
    Plain,

    /// Dictionary: valor → índice u8/u16
    /// Ideal para ENUMs, status, país (≤256 valores distintos)
    /// Ahorro típico: 'pendiente'(9 bytes) → 0u8(1 byte) = 9x
    Dictionary {
        dict: Vec<Value>,
        codes: Vec<u8>,   // u8 si ≤256 valores, u16 si ≤65536
    },

    /// Run-Length Encoding: (valor, count)
    /// Ideal para datos ordenados con muchos repetidos
    /// Ej: [A,A,A,A,B,B,C] → [(A,4),(B,2),(C,1)]
    RunLength {
        runs: Vec<(Value, u32)>,
    },

    /// Delta: guardar diferencias entre valores consecutivos
    /// Ideal para timestamps, IDs secuenciales
    /// Ej: [1000,1001,1002,1005] → [1000, +1, +1, +3]
    Delta {
        base:   i64,
        deltas: Vec<i16>,   // diferencias pequeñas en 2 bytes
    },

    /// BitPacking: n bits por valor (para enteros en rango pequeño)
    /// Ej: años 2020-2030 → 4 bits c/u → 2 valores por byte
    BitPacking {
        bits_per_value: u8,
        data:           Vec<u8>,
    },

    /// Frame Of Reference: base + delta pequeño
    /// Ideal para timestamps en ventana de tiempo
    FrameOfReference {
        base:   i64,
        deltas: Vec<i32>,
    },
}

impl ColumnEncoding {
    /// El query planner elige encoding automáticamente al crear la columna
    fn choose_best(samples: &[Value]) -> Self {
        let cardinality = unique_count(samples);
        let sorted = is_sorted(samples);

        if cardinality <= 256 {
            Self::Dictionary { .. }          // status, país, categoría
        } else if sorted && is_numeric(samples) {
            Self::Delta { .. }               // timestamps, IDs
        } else if sorted && has_long_runs(samples) {
            Self::RunLength { .. }           // datos agrupados
        } else if fits_in_n_bits(samples, 8) {
            Self::BitPacking { bits_per_value: 8, .. }
        } else {
            Self::Plain
        }
    }
}
```

---

### DECIMAL exacto — nunca usar FLOAT para dinero

```rust
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

// El problema del float:
let precio: f64 = 0.1 + 0.2;
assert_ne!(precio, 0.3);  // 0.30000000000000004 — ¡ERROR!

// Con DECIMAL:
let precio = dec!(0.1) + dec!(0.2);
assert_eq!(precio, dec!(0.3));  // exacto siempre

// Operaciones financieras seguras
let subtotal = dec!(19.99) * dec!(3);          // 59.97
let iva      = subtotal * dec!(0.19);          // 11.3943
let total    = subtotal + iva.round_dp(2);     // 71.37
```

---

### TIMESTAMPTZ — siempre UTC internamente

```rust
// El problema de TIMESTAMP sin zona:
// Servidor en UTC+0 → cliente en UTC-5 → insertan "2026-03-21 10:00"
// ¿A qué hora fue? Imposible saberlo.

// TIMESTAMPTZ: siempre UTC en disco, convierte al mostrar
use time::{OffsetDateTime, UtcOffset};

fn store_timestamptz(dt: OffsetDateTime) -> i64 {
    dt.unix_timestamp_nanos() / 1000  // guardar en µs UTC
}

fn display_timestamptz(micros: i64, client_tz: UtcOffset) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(micros / 1_000_000)
        .unwrap()
        .to_offset(client_tz)  // convertir al timezone del cliente al mostrar
}
```

---

### UUID v7 — mejor que v4 para PKs

```rust
// UUID v4: totalmente aleatorio → inserts en posiciones aleatorias del B+ Tree
// → muchos page splits → fragmentación → más I/O

// UUID v7: timestamp (48 bits) + random (80 bits) → ordenable por tiempo
// → inserts casi secuenciales → menos splits → mejor rendimiento como PK

use uuid::Uuid;

fn gen_pk() -> [u8; 16] {
    Uuid::now_v7().into_bytes()
    // primeros 48 bits = timestamp ms → ordenable
    // últimos 80 bits = random → único
}

// Comparación de rendimiento con 1M inserts:
// UUID v4 PK: ~150k inserts/s (muchos splits)
// UUID v7 PK: ~250k inserts/s (inserts casi secuenciales)
// BIGINT PK:  ~280k inserts/s (completamente secuencial)
```

---

### INTERVAL — duración correcta

```rust
// Por qué INTERVAL necesita meses + días + µs separados:
// '1 month' no son 30 días fijos (enero=31, febrero=28/29)
// '1 day' no son 86400 segundos fijos (DST puede añadir/quitar 1h)

struct DbInterval {
    months: i32,   // años*12 + meses
    days:   i32,   // días exactos
    micros: i64,   // horas*3.6B + minutos*60M + segundos*1M + µs
}

// '1 year 2 months 3 days 4 hours 30 minutes'
// → DbInterval { months: 14, days: 3, micros: 16_200_000_000 }

impl DbInterval {
    fn add_to_timestamp(&self, ts: OffsetDateTime) -> OffsetDateTime {
        ts.checked_add(Duration::days(self.days as i64))
          .unwrap()
          .checked_add(Duration::microseconds(self.micros))
          .unwrap()
        // meses: usar calendario (no aritmética fija)
    }
}
```

---

### RANGE types — solapamientos y contención

```sql
-- Rango de enteros
SELECT '[1,10)'::int4range @> 5;      -- true  (5 está en [1,10))
SELECT '[1,5)' && '[3,8)'::int4range; -- true  (se solapan)

-- Reservaciones sin solapamiento (constraint de exclusión)
CREATE TABLE salas (
  sala_id  INT,
  periodo  TSRANGE,
  EXCLUDE USING gist(sala_id WITH =, periodo WITH &&)
  -- dos reservas de la misma sala no pueden solaparse
);

INSERT INTO salas VALUES (1, '[2026-03-21 09:00, 2026-03-21 11:00)');
INSERT INTO salas VALUES (1, '[2026-03-21 10:00, 2026-03-21 12:00)');
-- ERROR: conflicto de exclusión (periodo se solapa)
```

```rust
#[derive(Clone)]
struct DbRange<T: Clone + PartialOrd> {
    lower:      Option<T>,
    upper:      Option<T>,
    lower_incl: bool,    // [ vs (
    upper_incl: bool,    // ] vs )
}

impl<T: Clone + PartialOrd> DbRange<T> {
    fn contains(&self, val: &T) -> bool {
        let lo_ok = match &self.lower {
            None => true,
            Some(lo) => if self.lower_incl { val >= lo } else { val > lo },
        };
        let hi_ok = match &self.upper {
            None => true,
            Some(hi) => if self.upper_incl { val <= hi } else { val < hi },
        };
        lo_ok && hi_ok
    }

    fn overlaps(&self, other: &Self) -> bool {
        !(self.upper_lt_lower_of(other) || other.upper_lt_lower_of(self))
    }
}
```

---

## Optimizaciones del Sistema de Tipos

### 1. VarInt Encoding — enteros con tamaño variable

En vez de siempre reservar 8 bytes para BIGINT, usar solo los bytes necesarios.
El 90% de IDs y contadores son pequeños — ahorro inmediato.

```
Valor         Bytes usados  Overhead vs BIGINT fijo
──────────────────────────────────────────────────
0 – 127            1 byte        -87%
128 – 16383        2 bytes       -75%
16384 – 2097151    3 bytes       -62%
2097152 – 268M     4 bytes       -50%
> 268M             5-9 bytes     variable

ID típico (42) → 1 byte en vez de 8 = 87% de ahorro en IDs
```

```rust
fn encode_varint(mut val: u64, buf: &mut Vec<u8>) {
    loop {
        let byte = (val & 0x7F) as u8;
        val >>= 7;
        if val == 0 {
            buf.push(byte);          // último byte: bit alto = 0
            break;
        }
        buf.push(byte | 0x80);       // hay más bytes: bit alto = 1
    }
}

fn decode_varint(buf: &[u8]) -> (u64, usize) {
    let mut val = 0u64;
    let mut shift = 0;
    for (i, &byte) in buf.iter().enumerate() {
        val |= ((byte & 0x7F) as u64) << shift;
        if byte & 0x80 == 0 { return (val, i + 1); }
        shift += 7;
    }
    panic!("varint truncado")
}

// Zigzag para enteros con signo (evita que negativos usen 9 bytes)
fn zigzag_encode(n: i64) -> u64 { ((n << 1) ^ (n >> 63)) as u64 }
fn zigzag_decode(n: u64) -> i64 { ((n >> 1) as i64) ^ -((n & 1) as i64) }
```

---

### 2. DECIMAL Fast Path — sin heap para ≤18 dígitos

```rust
// La mayoría de valores monetarios caben en i64 con escala fija.
// Solo escalar a BigDecimal si realmente se necesitan >18 dígitos.

enum Decimal {
    // Fast path: aritmética de enteros pura, sin heap, SIMD-friendly
    // Cubre DECIMAL(1,0) hasta DECIMAL(18,18)
    Small {
        mantissa: i64,  // 19.99 con scale=2 → 1999
        scale:    u8,   // número de decimales (0-18)
    },
    // Slow path: solo para DECIMAL(>18, x)
    Big(Box<rust_decimal::Decimal>),
}

impl Decimal {
    fn add(&self, other: &Self) -> Self {
        match (self, other) {
            // Misma escala: suma directa de i64 (1 instrucción CPU)
            (Small { mantissa: a, scale: sa }, Small { mantissa: b, scale: sb }) if sa == sb => {
                Small { mantissa: a + b, scale: *sa }
            }
            // Escalas distintas: alinear primero
            (Small { mantissa: a, scale: sa }, Small { mantissa: b, scale: sb }) => {
                let (a2, b2, scale) = align_scales(*a, *sa, *b, *sb);
                Small { mantissa: a2 + b2, scale }
            }
            // Cualquiera es Big: convertir ambos
            _ => Big(Box::new(self.to_big() + other.to_big())),
        }
    }
}
// Benchmark: Decimal::Small suma es ~20x más rápida que rust_decimal completo
```

---

### 3. JSONB — formato binario pre-parseado

JSON texto re-parsea en cada acceso. JSONB guarda en binario con tabla de offsets.

```
JSON texto:  '{"edad":25,"nombre":"Ana","rol":"admin"}'
             → parsear O(n) en cada query = lento

JSONB binario:
  [header: 3 keys] [offset_tabla: edad@12, nombre@16, rol@24]
  [keys sorted: "edad\0nombre\0rol\0"]
  [values: 25][3:"Ana"][5:"admin"]
  → acceso a campo: binary search sobre keys = O(log k) sin parsear = rápido
```

```rust
struct JsonbHeader {
    n_keys:  u32,
    offsets: Vec<u32>,   // posición de cada key en el buffer
}

fn jsonb_get(data: &[u8], key: &str) -> Option<Value> {
    let header = JsonbHeader::parse(data);
    let idx = header.binary_search_key(data, key)?;  // O(log n)
    Some(parse_value_at(data, header.offsets[idx]))
}

fn json_to_jsonb(json: &serde_json::Value) -> Vec<u8> {
    let mut keys: Vec<&str> = json.as_object()?.keys().map(String::as_str).collect();
    keys.sort();   // ordenar keys para binary search
    // serializar header + offsets + keys + values
    serialize_jsonb(keys, json)
}
// Acceso a campo individual: 10-50x más rápido que JSON texto
```

---

### 4. VECTOR — cuantización para ahorrar espacio

```sql
-- f32 completo: 4 bytes/dim — máxima precisión
CREATE TABLE docs (embedding VECTOR(1536));               -- 6,144 bytes/fila

-- f16 media precisión: 2 bytes/dim — pérdida <1% en similitud coseno
CREATE TABLE docs (embedding VECTOR(1536) PRECISION float16);  -- 3,072 bytes/fila

-- int8 cuantizado: 1 byte/dim — pérdida ~2-3% — ultra-rápido con SIMD
CREATE TABLE docs (embedding VECTOR(1536) PRECISION int8);     -- 1,536 bytes/fila
```

```rust
// Cuantización scalar: mapear f32 [-1.0, 1.0] → i8 [-128, 127]
fn quantize_f32_to_i8(vec: &[f32]) -> (Vec<i8>, f32) {
    let scale = vec.iter().map(|x| x.abs()).cloned().fold(0.0f32, f32::max);
    let quantized = vec.iter().map(|x| (x / scale * 127.0) as i8).collect();
    (quantized, scale)   // guardar scale factor para dequantizar
}

fn dequantize_i8_to_f32(quantized: &[i8], scale: f32) -> Vec<f32> {
    quantized.iter().map(|&x| x as f32 / 127.0 * scale).collect()
}

// Similitud coseno con int8: AVX2 procesa 32 valores por instrucción
// vs f32: solo 8 valores por instrucción → 4x más rápido en búsqueda
fn dot_product_i8_simd(a: &[i8], b: &[i8]) -> i32 {
    a.iter().zip(b).map(|(&x, &y)| x as i32 * y as i32).sum()
}
```

---

### 5. SIMD por tipo — elegir el tipo mínimo adecuado

```
Tipo       Valores por instrucción AVX2   Cuándo usar
──────────────────────────────────────────────────────────────────
TINYINT     32 valores                   age, rating, status numérico
SMALLINT    16 valores                   año, código, cantidad pequeña
INT          8 valores                   ID normal, precio en centavos
BIGINT       4 valores                   ID de alta escala, timestamp
REAL         8 valores (FMA)             coordenadas, scores de ML
DOUBLE       4 valores (FMA)             cálculos científicos

→ Usar TINYINT en vez de INT cuando los datos caben = 4x más rápido en scans
→ El query planner puede sugerir el tipo mínimo al hacer ANALYZE
```

```rust
// El planner detecta si una columna INT podría ser TINYINT
fn suggest_smaller_type(stats: &ColumnStats) -> Option<DataType> {
    match stats.column_type {
        DataType::Int => {
            if stats.min >= -128 && stats.max <= 127 {
                Some(DataType::TinyInt)  // 4x SIMD speedup
            } else if stats.min >= -32768 && stats.max <= 32767 {
                Some(DataType::SmallInt) // 2x SIMD speedup
            } else { None }
        }
        _ => None,
    }
}
```

---

### 6. PAX Layout — columnar dentro de cada página

Mejor cache locality para analytics sin cambiar el formato de archivo.

```
Row layout (actual):
  Página: [id1|age1|name1] [id2|age2|name2] [id3|age3|name3]
  SELECT AVG(age): lee id+name también aunque no se necesitan

PAX (Partition Attributes Across):
  Página: [ids: 1|2|3|...] [ages: 25|30|22|...] [names: "Ana"|"Bob"|...]
  SELECT AVG(age): solo lee el bloque de ages → 3x menos I/O
  SIMD en ages: todos i32 contiguos → procesar 8 a la vez
```

```rust
struct PaxPage {
    header:  PageHeader,
    // columnas almacenadas contiguamente dentro de la página
    columns: Vec<ColumnSlice>,
}

struct ColumnSlice {
    col_idx:  u16,
    encoding: ColumnEncoding,
    data:     Range<usize>,   // offset dentro del buffer de la página
}

// Leer solo la columna 'age' sin tocar las otras
fn read_column(page: &PaxPage, col: usize) -> &[i32] {
    let slice = &page.columns[col];
    bytemuck::cast_slice(&page.buffer[slice.data.clone()])
}
```

---

### 7. Estadísticas por columna — planner inteligente

```rust
#[derive(Debug, Clone)]
struct ColumnStats {
    // Básicas
    null_frac:     f64,              // 0.0 a 1.0 — fracción de NULLs
    n_distinct:    i64,              // >0 = conteo exacto, -1 = todos distintos
    avg_width:     u32,              // bytes promedio (útil para TEXT)
    row_count:     u64,

    // Distribución
    min:           Value,
    max:           Value,
    histogram:     Vec<Value>,       // N buckets de igual frecuencia
    most_common:   Vec<(Value,f64)>, // [(valor, frecuencia)] top-K

    // Para el planner de índices
    correlation:   f64,              // -1.0 a 1.0: qué tan ordenada está
    // correlation ≈ 1.0  → index scan muy eficiente (datos casi ordenados)
    // correlation ≈ 0.0  → index scan caro (random I/O), preferir seq scan
}

// Uso en el planner:
// WHERE age BETWEEN 20 AND 30
// → histogram dice 15% selectividad
// → correlation(age) = 0.95 → index scan (datos casi ordenados por age)
// → estimado: 150K filas × 0.15 = 22.5K filas → index scan

// UPDATE a miles de filas con correlation ≈ 0 → seq scan más barato
fn choose_scan(stats: &ColumnStats, selectivity: f64) -> ScanType {
    let index_cost = selectivity * stats.row_count as f64
        * (1.0 - stats.correlation.abs()) * RANDOM_PAGE_COST;
    let seq_cost   = stats.row_count as f64 * SEQ_PAGE_COST;
    if index_cost < seq_cost { ScanType::Index } else { ScanType::Sequential }
}
```

```sql
-- Actualizar estadísticas manualmente
ANALYZE users;
ANALYZE users (age, email);   -- solo columnas específicas

-- Ver estadísticas actuales
SELECT * FROM db_column_stats WHERE table_name = 'users';
```

---

### 8. Cotejamiento (Collation) — sistema completo

#### El problema real

```sql
-- Sin cotejamiento correcto, estas queries fallan silenciosamente:
SELECT * FROM users WHERE nombre = 'jose';    -- NO encuentra 'José'
SELECT * FROM users WHERE email LIKE 'ana%';  -- NO encuentra 'Ana@gmail.com'
SELECT * FROM users ORDER BY nombre;          -- 'Álvarez' aparece después de 'Zuniga'
SELECT COUNT(*) FROM users GROUP BY nombre;   -- 'jose' y 'José' son dos grupos distintos
UPPER('josé')    -- da 'JOSé' sin Unicode (la é no se convierte)
LENGTH('José')   -- puede dar 4 o 5 según normalización Unicode
```

#### Niveles Unicode CLDR

```
Nivel 1 — Primary:     ignora acentos Y mayúsculas  → a = á = A = Á
Nivel 2 — Secondary:   distingue acentos, no mayúsculas → a = A, a ≠ á
Nivel 3 — Tertiary:    distingue acentos Y mayúsculas → a ≠ A, a ≠ á  (default)
Nivel 4 — Quaternary:  distingue todo + puntuación y espacios
```

```sql
-- Sufijos de collation
_ci   → case-insensitive     (a = A)
_cs   → case-sensitive       (a ≠ A)
_ai   → accent-insensitive   (a = á)
_as   → accent-sensitive     (a ≠ á)
_bin  → binario exacto       (byte a byte)

-- Combinaciones comunes
es_419_ci    → español latinoamérica, case-insensitive
es_419_ci_ai → español latinoamérica, sin importar mayúsculas ni acentos
utf8_bin     → binario, máxima precisión (para emails, tokens, hashes)
en_US_ci     → inglés EE.UU., case-insensitive
```

#### Configuración en cascada

```sql
-- Nivel 1: servidor (default para todo)
-- dbyo.toml
-- [server]
-- default_charset   = "utf8mb4"
-- default_collation = "es_419_ci"

-- Nivel 2: base de datos
CREATE DATABASE biblia
  CHARACTER SET utf8mb4
  COLLATE       es_419_ci;

-- Nivel 3: tabla
CREATE TABLE users (
  id     INT PRIMARY KEY,
  nombre TEXT,              -- hereda es_419_ci de la BD
  email  TEXT COLLATE utf8_bin,     -- override: binario exacto
  notas  TEXT COLLATE es_419_ci_ai  -- override: ignora acentos también
) COLLATE es_419_ci;

-- Nivel 4: columna individual (mayor prioridad)
ALTER TABLE users MODIFY email TEXT COLLATE utf8_bin;

-- Nivel 5: query (mayor prioridad de todas)
SELECT * FROM users WHERE nombre COLLATE es_419_ci = 'jose';
-- encuentra: 'Jose', 'JOSE', 'José', 'josé', 'josè'
```

#### Encodings soportados

```sql
SHOW CHARACTER SETS;
-- utf8mb4   → UTF-8 completo (1-4 bytes, emojis incluidos) ← recomendado
-- utf8      → UTF-8 limitado a 3 bytes (sin emojis — legado)
-- latin1    → ISO-8859-1 (Europa occidental — legado)
-- ascii     → 7 bits (solo inglés)
-- utf16     → UTF-16 (Windows/Java legacy)
-- binary    → bytes crudos

-- Internamente siempre UTF-8 (Rust nativo)
-- Conversión automática en el wire protocol si el cliente pide otro encoding
```

#### Unicode Normalization — el problema oculto

```
La 'é' puede representarse de DOS formas válidas en Unicode:
  NFC:  U+00E9           → un solo codepoint "é precompuesto"
  NFD:  U+0065 + U+0301  → "e" + combining accent (dos codepoints)

Son el mismo carácter visual pero bytes distintos:
  'é'(NFC) == 'é'(NFD)  → FALSE sin normalizar
  LENGTH('é' NFC) = 1, LENGTH('é' NFD) = 2

Solución: normalizar siempre a NFC antes de guardar
```

```rust
use unicode_normalization::UnicodeNormalization;

fn normalize_for_storage(s: &str) -> String {
    s.nfc().collect()      // NFC: forma canónica compuesta
}

fn normalize_for_search(s: &str) -> String {
    s.nfkc().collect()     // NFKC: además aplana ligaduras (ﬁ→fi, ™→TM, ½→1/2)
}

fn strip_accents(s: &str) -> String {
    // NFD separa carácter + acento, luego filtrar combining marks
    s.nfd()
     .filter(|c| !unicode_normalization::char::is_combining_mark(*c))
     .collect()
    // 'José' → 'Jose', 'Ñoño' → 'Nono'
}
```

Crate: `unicode-normalization = "0.1"`.

#### Implementación del CollationEngine

```rust
use icu_collator::{Collator, CollatorOptions, Strength, CaseLevel};
use icu_locid::locale;

struct CollationDef {
    locale:           String,
    strength:         Strength,      // Primary/Secondary/Tertiary/Quaternary
    case_sensitive:   bool,
    accent_sensitive: bool,
    binary:           bool,
}

struct CollationEngine {
    collators: HashMap<String, Collator>,
    cache:     LruCache<(String, String), Vec<u8>>,  // sort key cache
}

impl CollationEngine {
    fn new() -> Self {
        let mut engine = Self { collators: HashMap::new(), cache: LruCache::new(10_000) };

        // Pre-cargar collations más comunes
        for (name, loc) in [
            ("es_419_ci", locale!("es-419")),
            ("es_419_cs", locale!("es-419")),
            ("en_US_ci",  locale!("en-US")),
            ("utf8_bin",  locale!("und")),
        ] {
            let mut opts = CollatorOptions::new();
            if name.ends_with("_ci") { opts.strength = Some(Strength::Secondary); }
            if name.ends_with("_ai") { opts.strength = Some(Strength::Primary); }
            engine.collators.insert(name.to_string(), Collator::try_new(loc, opts).unwrap());
        }
        engine
    }

    fn compare(&self, collation: &str, a: &str, b: &str) -> std::cmp::Ordering {
        if collation == "utf8_bin" || collation == "binary" {
            return a.cmp(b);  // fast path: comparación binaria directa
        }
        self.collators[collation].compare(a, b)
    }

    fn sort_key(&mut self, collation: &str, s: &str) -> Vec<u8> {
        let cache_key = (collation.to_string(), s.to_string());
        if let Some(key) = self.cache.get(&cache_key) {
            return key.clone();
        }
        let key = if collation == "utf8_bin" {
            s.as_bytes().to_vec()
        } else {
            self.collators[collation].sort_key(s).to_vec()
        };
        // Guardar en el B+ Tree en vez del string original
        self.cache.put(cache_key, key.clone());
        key
    }
}
```

#### Impacto en el B+ Tree — sort keys en las hojas

```rust
// Las hojas del B+ Tree guardan la SORT KEY del collation, no el string raw
// Así ORDER BY, =, <, >, BETWEEN funcionan correctamente con memcmp

struct BTreeLeaf {
    // Para columnas con collation:
    sort_keys: Vec<Vec<u8>>,   // sort_key del collation (comparable con memcmp)
    values:    Vec<RecordId>,  // RID de la fila real

    // Para recuperar el valor original:
    // leer la fila completa desde la tabla principal
}

fn insert_with_collation(
    tree: &mut BTree,
    value: &str,
    rid: RecordId,
    collation: &str,
    engine: &mut CollationEngine,
) {
    let sort_key = engine.sort_key(collation, value);
    tree.insert(sort_key, rid);
}

fn lookup_with_collation(
    tree: &BTree,
    query: &str,
    collation: &str,
    engine: &mut CollationEngine,
) -> Vec<RecordId> {
    let sort_key = engine.sort_key(collation, query);
    tree.lookup(&sort_key)
    // encuentra 'jose', 'José', 'JOSE' con collation es_419_ci
}
```

#### Funciones de string con collation

```rust
struct StringFunctions {
    engine:    CollationEngine,
    normalizer: UnicodeNormalizer,
}

impl StringFunctions {
    // UPPER/LOWER respetan Unicode completo
    fn upper(s: &str, locale: &str) -> String {
        // ICU4X locale-aware uppercase
        // UPPER('josé') en es_419 → 'JOSÉ' (no 'JOSé')
        icu_casemap::CaseMapper::new().uppercase_to_string(s, &locale.parse().unwrap())
    }

    fn lower(s: &str, locale: &str) -> String {
        icu_casemap::CaseMapper::new().lowercase_to_string(s, &locale.parse().unwrap())
    }

    // LENGTH en codepoints, no en bytes
    fn char_length(s: &str) -> usize {
        s.chars().count()   // 'José' → 4, no 5
    }

    // LIKE respeta collation
    fn like_match(pattern: &str, value: &str, collation: &str, engine: &CollationEngine) -> bool {
        // Convertir LIKE pattern a regex con collation
        let regex = like_pattern_to_regex(pattern, collation);
        regex.is_match(value)
        // 'jos%' con es_419_ci encuentra 'José González'
    }

    // SOUNDEX en español
    fn soundex_es(s: &str) -> String {
        // Algoritmo Soundex adaptado para español
        // 'Jorge' → 'J620', 'George' → 'J620' (mismo código)
        soundex::encode_es(s)
    }

    // SIMILARITY con trigramas respeta collation
    fn similarity(a: &str, b: &str, collation: &str) -> f32 {
        let a_norm = normalize_for_search(a);
        let b_norm = normalize_for_search(b);
        trigram_similarity(&a_norm, &b_norm)
    }
}
```

#### Collations soportados

```sql
SHOW COLLATIONS;
-- Nombre            Charset   Descripción
-- es_419_ci         utf8mb4   Español latinoamérica, case-insensitive
-- es_419_cs         utf8mb4   Español latinoamérica, case-sensitive
-- es_419_ci_ai      utf8mb4   Español latinoamérica, sin acentos ni mayúsculas
-- es_ES_ci          utf8mb4   Español España (LL y CH como letras)
-- en_US_ci          utf8mb4   Inglés EE.UU., case-insensitive
-- en_US_cs          utf8mb4   Inglés EE.UU., case-sensitive
-- pt_BR_ci          utf8mb4   Portugués Brasil
-- fr_FR_ci          utf8mb4   Francés
-- de_DE_ci          utf8mb4   Alemán (ß = ss)
-- zh_Hans_ci        utf8mb4   Chino simplificado (por pinyin)
-- zh_Hant_ci        utf8mb4   Chino tradicional
-- ja_ci             utf8mb4   Japonés
-- ar_ci             utf8mb4   Árabe
-- utf8mb4_unicode   utf8mb4   Unicode genérico
-- utf8_bin          utf8mb4   Binario exacto (byte a byte)
-- ascii_bin         ascii     ASCII binario

-- Para la app de Biblia: es_419_ci_ai (ignora mayúsculas Y acentos)
-- Búsqueda de 'amos' encuentra 'Amós', 'AMÓS', 'amos'
```

#### Detección automática de encoding del cliente

```rust
async fn handle_connection(stream: TcpStream, engine: Arc<Engine>) {
    let startup = read_mysql_handshake(&stream).await;

    // El cliente declara su charset en el handshake
    let client_charset = startup.charset;   // ej: latin1, utf8, utf8mb4

    let conn = Connection {
        charset:   client_charset,
        collation: engine.default_collation(),
        // Convertir en cada mensaje si client_charset != utf8mb4
        transcoder: if client_charset != "utf8mb4" {
            Some(Transcoder::new(client_charset, "utf8mb4"))
        } else {
            None
        },
    };
}

struct Transcoder {
    from: &'static encoding_rs::Encoding,
    to:   &'static encoding_rs::Encoding,
}

impl Transcoder {
    fn decode(&self, bytes: &[u8]) -> String {
        let (decoded, _, _) = self.from.decode(bytes);
        decoded.nfc().collect()   // + normalizar a NFC
    }
}
```

Crate: `encoding_rs = "0.8"` para conversión de charsets legacy (latin1, windows-1252).

#### Collation en el FTS (Full-Text Search)

```rust
struct FtsTokenizerWithCollation {
    collation:  String,
    engine:     CollationEngine,
    stop_words: HashMap<String, HashSet<String>>, // locale → stop words
}

impl FtsTokenizerWithCollation {
    fn tokenize(&mut self, text: &str, locale: &str) -> Vec<String> {
        let normalized = normalize_for_search(text); // NFKC
        let stop = &self.stop_words[locale];

        normalized
            .split(|c: char| !c.is_alphabetic())
            .filter(|t| !t.is_empty())
            .map(|t| StringFunctions::lower(t, locale))  // lowercase con ICU4X
            .filter(|t| !stop.contains(t.as_str()))
            .map(|t| stem(t, locale))                    // stemming por idioma
            .collect()
    }
}

// En la app de Biblia:
// buscar 'amos' con collation es_419_ci_ai
// tokenizer normaliza → 'amos'
// encuentra versículos con 'Amós', 'AMÓS', 'amós', 'amos'
```

Crates: `icu_collator = "1"`, `icu_casemap = "1"`, `unicode-normalization = "0.1"`, `encoding_rs = "0.8"`.

---

### 9. Zero-copy deserialization con rkyv

Para nodos del B+ Tree: evitar deserialización al leer del mmap.

```rust
use rkyv::{Archive, Deserialize, Serialize, rancor::Error};

#[derive(Archive, Serialize, Deserialize)]
struct BTreeNode {
    is_leaf:  bool,
    num_keys: u16,
    keys:     [i64; 200],
    children: [u64; 201],
}

// Con bytemuck: cast directo desde bytes del mmap — CERO copias
fn read_node_zero_copy(page: &[u8]) -> &ArchivedBTreeNode {
    unsafe { rkyv::access_unchecked::<BTreeNode>(page) }
    // La estructura vive en el mmap — no se copia nada a heap
}

// Comparación:
// Con serde:     leer 8KB → allocar struct → deserializar → usar  (~500ns)
// Con zero-copy: leer 8KB → cast puntero → usar directamente    (~10ns)
// 50x más rápido en lecturas de nodos frecuentes
```

Crate: `rkyv = "0.8"`.

---

### 10. Compresión específica por tipo

```rust
enum TypeCompression {
    None,
    Lz4,    // bloques binarios — descompresión ultrarrápida (1GB/s)
    Zstd,   // texto largo, JSON — mejor ratio que LZ4
    Delta,  // timestamps, IDs secuenciales — 80-95% ahorro
    BitPack,// booleans, enums pequeños — 8-32x ahorro
}

fn choose_compression(col_type: &DataType, stats: &ColumnStats) -> TypeCompression {
    match col_type {
        // Timestamps casi secuenciales → Delta encoding
        DataType::Timestamp if stats.correlation > 0.8 => TypeCompression::Delta,

        // Texto largo → ZSTD (mejor ratio)
        DataType::Text if stats.avg_width > 500 => TypeCompression::Zstd,

        // JSON → ZSTD (tiene mucho texto redundante)
        DataType::Json => TypeCompression::Zstd,

        // BLOB binario → LZ4 (descompresión rápida)
        DataType::Bytes => TypeCompression::Lz4,

        // Bool y enums pequeños → BitPacking
        DataType::Bool | DataType::TinyInt => TypeCompression::BitPack,

        // Alta cardinalidad sin patrón → sin compresión
        _ if stats.n_distinct < 0 => TypeCompression::None,

        _ => TypeCompression::Lz4,
    }
}
```

---

## Storage Engine

### Formato de página (8KB, cache-line aligned)

```rust
#[repr(C, align(64))]
struct Page {
    page_id:   u64,        // identificador único
    page_type: PageType,   // BTreeNode | Data | Overflow | Free
    lsn:       u64,        // Log Sequence Number (para WAL)
    checksum:  u32,        // CRC32 para detectar torn pages
    data:      [u8; 8168], // payload
}

enum PageType {
    BTreeInternal,  // nodo interno del árbol (solo keys + punteros)
    BTreeLeaf,      // hoja (keys + datos o RID)
    Data,           // página de datos sin orden
    Overflow,       // valores grandes que no caben en una página
}
```

### mmap del archivo de base de datos

```rust
struct StorageEngine {
    mmap:     MmapMut,       // archivo .db completo en memoria virtual
    wal:      WalWriter,     // append-only para durabilidad
    root_ptr: AtomicU64,     // page_id del root (swap atómico para CoW)
    free_list: Mutex<Vec<u64>>, // páginas liberadas reutilizables
}
```

El `mmap` mapea el archivo completo. El OS kernel maneja qué páginas
están en RAM y cuáles en disco. No necesitamos buffer pool propio.

---

## B+ Tree Copy-on-Write

### Concepto

```
Estado inicial:          Después de un write:

      [Root v1]               [Root v2]  ← nuevo root atómico
      /        \              /        \
  [N1]        [N2]        [N1]        [N2']  ← copia modificada
  /  \        /  \        /  \        /  \
[L1][L2]  [L3][L4]    [L1][L2]  [L3][L4']  ← hoja nueva

Readers con referencia al Root v1 ven datos consistentes.
No hay locks. El swap del root es un CAS atómico (1 instrucción).
```

### Estructura de nodo

```rust
struct BTreeNode {
    is_leaf:   bool,
    num_keys:  u16,
    keys:      [KeyType; ORDER],       // ORDER = 200 para páginas 8KB
    // Si es hoja:
    values:    [RecordId; ORDER],      // (page_id, slot_id)
    next_leaf: Option<u64>,            // linked list de hojas para range scans
    // Si es interno:
    children:  [u64; ORDER + 1],       // page_ids de hijos
}
```

### Operaciones

| Operación | Complejidad | Locks necesarios |
|---|---|---|
| Lookup por key | O(log n) | Ninguno (readers libres) |
| Range scan | O(log n + k) | Ninguno |
| Insert | O(log n) | Solo en path del root al nodo |
| Delete | O(log n) | Solo en path del root al nodo |

---

## WAL (Write-Ahead Log)

```
Cada write va primero al WAL (append secuencial = rápido):

  [ LSN | Type | Table | Key | OldValue | NewValue | CRC ]

Flush del WAL al .db ocurre en checkpoint (cada N segundos o M bytes).

Crash recovery:
  1. Leer WAL desde último checkpoint
  2. Reaplicar operaciones committed
  3. Descartar operaciones sin COMMIT
```

**Ventaja vs MySQL:** Sin double-write buffer. Una sola escritura secuencial.
Las escrituras secuenciales en SSD son 3-5x más rápidas que random writes.

---

## Índices

### Por tabla

```
tabla users:
  ├── primary.btree    → B+ Tree por PRIMARY KEY (id)
  ├── email.btree      → B+ Tree por email (UNIQUE)
  ├── role_id.btree    → B+ Tree para FK lookup y JOIN
  └── name.btree       → B+ Tree para búsquedas por nombre
```

### SQL para crear índices

```sql
CREATE TABLE users (
  id       INT PRIMARY KEY,
  name     VARCHAR(100) NOT NULL,
  email    VARCHAR(200) UNIQUE,
  role_id  INT REFERENCES roles(id)
);

-- Índice explícito
CREATE INDEX idx_users_name ON users(name);

-- Índice compuesto
CREATE INDEX idx_orders_client_date ON orders(client_id, created_at DESC);
```

### Índice inverso para Foreign Keys

```
roles.id → [users.role_id index]

Cuando haces DELETE FROM roles WHERE id = 5:
  1. Buscar en índice inverso: ¿hay users con role_id = 5?
  2. Si hay → ERROR (RESTRICT) o eliminar en cascada (CASCADE)
  3. Lookup es O(log n), no full scan
```

---

## Llaves Foráneas

```sql
-- Definición
CREATE TABLE orders (
  id        INT PRIMARY KEY,
  client_id INT NOT NULL REFERENCES clients(id) ON DELETE RESTRICT,
  product_id INT REFERENCES products(id) ON DELETE SET NULL
);
```

### Comportamientos soportados

| Acción | Descripción |
|---|---|
| `RESTRICT` | Error si hay hijos al eliminar padre |
| `CASCADE` | Elimina hijos automáticamente |
| `SET NULL` | Pone NULL en la FK del hijo |
| `NO ACTION` | Igual que RESTRICT (default) |

### Implementación

```
INSERT INTO orders (client_id = 99):
  1. Lookup en clients.primary.btree → key 99 existe? → OK
  2. Si no existe → error "FK violation"

DELETE FROM clients WHERE id = 99:
  1. Buscar en orders.client_id.btree → hay rows con client_id = 99?
  2. Según ON DELETE: RESTRICT → error | CASCADE → delete orders también
```

---

## Concurrencia — MVCC

Cada fila tiene metadata de transacción:

```rust
struct RowHeader {
    txn_id_created:  u64,   // qué transacción creó esta fila
    txn_id_deleted:  u64,   // qué transacción la eliminó (0 = activa)
    row_version:     u32,   // versión para optimistic locking
}
```

### Reglas de visibilidad

```
Una fila es visible para transacción T si:
  - txn_id_created < T.snapshot_id   (fue creada antes del snapshot)
  - txn_id_deleted == 0              (no ha sido eliminada)
    O txn_id_deleted > T.snapshot_id (fue eliminada después del snapshot)
```

**Resultado:** Readers nunca bloquean writers. Writers nunca bloquean readers.
Solo writer vs writer se serializa.

---

## Ejecución Vectorizada

```rust
// En lugar de evaluar fila por fila:
fn scan_table_vectorized(table: &Table, predicate: &Predicate) -> Vec<Row> {
    let mut results = Vec::new();

    // Procesar en chunks de 1024 filas
    for chunk in table.chunks(1024) {
        // Cargar columna completa del chunk (layout columnar para el scan)
        let ages: [i32; 1024] = chunk.column("age");

        // Aplicar predicado con SIMD (AVX2: 8 i32 a la vez)
        let mask = simd_gt_i32(&ages, 25);  // 256 bits de resultado

        // Emitir solo las filas donde mask == 1
        for (i, bit) in mask.iter().enumerate() {
            if bit { results.push(chunk.row(i)); }
        }
    }
    results
}
```

---

## Optimizaciones inspiradas en otras bases de datos

### PostgreSQL — Partial Indexes

Indexar solo el subconjunto de filas que realmente se consulta.
Resultado: índice 10-100x más pequeño → menos RAM, inserts más rápidos.

```sql
-- Solo usuarios activos (si el 95% está inactivo, el índice es 20x más pequeño)
CREATE INDEX idx_active_users ON users(email) WHERE active = true;

-- Solo pedidos pendientes
CREATE INDEX idx_pending_orders ON orders(created_at) WHERE status = 'pending';
```

```rust
struct IndexDef {
    columns: Vec<usize>,
    predicate: Option<Expr>,  // None = índice completo, Some = partial
}

fn should_index_row(row: &Row, idx: &IndexDef) -> bool {
    match &idx.predicate {
        None => true,
        Some(pred) => pred.eval(row).as_bool(),
    }
}
```

---

### PostgreSQL — EXPLAIN ANALYZE

Mostrar el plan de ejecución real con tiempos medidos por nodo.

```sql
EXPLAIN ANALYZE SELECT u.name, r.name
FROM users u JOIN roles r ON u.role_id = r.id
WHERE u.active = true;

-- Salida esperada:
-- Hash Join  (cost=12.5..45.2)  actual time=0.4..2.1ms  rows=342
--   Hash Cond: (u.role_id = r.id)
--   -> Index Scan on users using idx_active_users
--        actual time=0.1..1.2ms  rows=342
--   -> Hash on roles  (cost=5.0..5.0)  rows=10  actual time=0.1ms
-- Planning: 0.3ms   Execution: 2.4ms
```

```rust
enum PlanNode {
    SeqScan   { table: String, rows_est: u64, cost: f64 },
    IndexScan { table: String, index: String, rows_est: u64, cost: f64 },
    HashJoin  { left: Box<PlanNode>, right: Box<PlanNode>, cost: f64 },
    Filter    { predicate: Expr, child: Box<PlanNode> },
}

struct ExplainResult {
    node: PlanNode,
    actual_rows: u64,
    actual_time_ms: f64,
    children: Vec<ExplainResult>,
}
```

---

### PostgreSQL — TOAST (valores grandes fuera de página)

Valores mayores a ~2KB se comprimen y mueven a páginas de overflow.
La fila principal guarda solo un puntero de 8 bytes. La página de 8KB nunca se fragmenta.

```
Fila normal:   [id=1][age=25][name="Ana"][bio="...2KB de texto..."]
               └── no cabe en una página de 8KB → fragmentación

Con TOAST:     [id=1][age=25][name="Ana"][bio_ptr → overflow_page_4521]
               └── fila pequeña, overflow en página separada

Overflow page: [toast_header][compressed_data: 2KB → 800 bytes con lz4]
```

```rust
const TOAST_THRESHOLD: usize = 2048; // >2KB va a overflow

fn store_value(val: &[u8], page_mgr: &mut PageManager) -> ValueRef {
    if val.len() <= TOAST_THRESHOLD {
        ValueRef::Inline(val.to_vec())
    } else {
        let compressed = lz4_compress(val);
        let page_id = page_mgr.alloc_overflow(&compressed);
        ValueRef::Toast { page_id, original_len: val.len() }
    }
}
```

Crate: `lz4_flex = "0.11"` — compresión LZ4 pura Rust, muy rápida.

---

### RocksDB — Bloom Filters por índice

Evitar I/O de disco para point lookups de keys que **no existen**.

```
Sin bloom filter:
  ¿Existe user_id=99999?  →  recorrer B+ Tree  →  3-5 lecturas de disco → NO

Con bloom filter:
  ¿Existe user_id=99999?  →  consultar filtro en RAM (200 bytes)  →  NO → 0 I/O
  ¿Existe user_id=1?      →  filtro dice "posiblemente sí"  →  confirmar en árbol
```

```rust
use bloomfilter::Bloom;

struct IndexBloom {
    filter: Bloom<u64>,     // ~10 bits por key → 1% false positive rate
    // False positive: dice "existe" pero no existe → 1 I/O extra (aceptable)
    // False negative: NUNCA ocurre → si dice "no existe", es seguro
}

impl IndexBloom {
    fn might_exist(&self, key: u64) -> bool {
        self.filter.check(&key)   // false → definitivamente no existe
    }
    fn add(&mut self, key: u64) {
        self.filter.set(&key)
    }
}
```

Crate: `bloomfilter = "1"`. Especialmente útil para validar FKs en INSERT.

---

### ClickHouse — Sparse Index

Índice que guarda solo 1 entrada cada N filas. Mucho más pequeño que B+ Tree completo.
Ideal para columnas de timestamp o columnas ordenadas con range scans frecuentes.

```
Dense index (B+ Tree):   [fila1→pág1][fila2→pág1][fila3→pág1]...[fila1M→pág125K]
                          ← 1M entradas en RAM

Sparse index:            [fila1→pág1][fila8193→pág2][fila16385→pág3]...
                          ← 122 entradas en RAM (8192x menos)

Para buscar fila 5000:   ir a entrada fila1 → scan lineal hasta fila 5000 (máx 8192 filas)
```

```rust
struct SparseIndex {
    granularity: u64,              // una entrada cada N filas (default: 8192)
    marks: Vec<(KeyType, u64)>,    // (primera_key_del_bloque, page_id)
}

impl SparseIndex {
    fn find_block(&self, key: &KeyType) -> u64 {
        // binary search en marks → página donde puede estar el key
        self.marks.partition_point(|(k, _)| k <= key).saturating_sub(1)
            .and_then(|i| self.marks.get(i))
            .map(|(_, page)| *page)
            .unwrap_or(0)
    }
}
```

---

### WiredTiger — Prefix Compression en B+ Tree

Los nodos del árbol guardan el prefijo común una sola vez.
Cada nodo puede almacenar 3-5x más keys → árbol más bajo → menos I/O.

```
Sin compresión (nodo interno):
  ["usuario:00001", "usuario:00002", "usuario:00003", "usuario:00004"]
   14 bytes cada uno → 56 bytes para 4 keys

Con prefix compression:
  prefix = "usuario:"
  suffixes = ["00001", "00002", "00003", "00004"]
  5 bytes cada suffix → 20 bytes + 8 prefijo = 28 bytes  (2x ahorro)
```

```rust
struct CompressedNode {
    common_prefix: Vec<u8>,
    suffixes: Vec<Vec<u8>>,         // solo la parte única de cada key
    children: Vec<u64>,             // page_ids
}

impl CompressedNode {
    fn reconstruct_key(&self, idx: usize) -> Vec<u8> {
        [self.common_prefix.as_slice(), self.suffixes[idx].as_slice()].concat()
    }

    fn find_common_prefix(keys: &[Vec<u8>]) -> Vec<u8> {
        keys.iter().fold(keys[0].clone(), |acc, k| {
            acc.iter().zip(k).take_while(|(a, b)| a == b)
               .map(|(a, _)| *a).collect()
        })
    }
}
```

---

### SQLite — In-Memory Mode

La BD vive en RAM pura. Sin WAL, sin mmap, sin disco. Para tests y datos temporales.

```rust
pub enum StorageBackend {
    Disk {
        mmap: MmapMut,
        wal: WalWriter,
        path: PathBuf,
    },
    Memory {
        pages: HashMap<u64, [u8; 8192]>,  // RAM directa
        next_page: u64,
    },
}

impl Database {
    pub fn open(path: &str) -> Result<Self> {
        match path {
            ":memory:" => Self::open_memory(),
            path       => Self::open_disk(path),
        }
    }
}
```

```sql
-- Uso en CLI / tests
OPEN :memory:
CREATE TABLE tmp (id INT, val TEXT);
INSERT INTO tmp VALUES (1, 'test');
-- Al cerrar, todo desaparece
```

---

### SQLite — JSON como tipo nativo

Columnas semiestructuradas sin esquema fijo. Almacenado como TEXT, con funciones de extracción.

```sql
CREATE TABLE events (
  id         INT PRIMARY KEY,
  created_at TIMESTAMP,
  data       JSON
);

INSERT INTO events VALUES (1, NOW(), '{"type": "click", "x": 150, "y": 200}');

-- Extraer campo del JSON
SELECT data->>'$.type', data->>'$.x'
FROM events
WHERE data->>'$.type' = 'click';

-- JSON en WHERE con índice parcial
CREATE INDEX idx_click_events ON events(created_at)
WHERE data->>'$.type' = 'click';
```

```rust
// JSON se guarda como TEXT/BLOB, se parsea en el executor
use serde_json::Value;

fn json_extract(json_str: &str, path: &str) -> Option<Value> {
    let val: Value = serde_json::from_str(json_str).ok()?;
    // path: "$.type" → val["type"]
    jsonpath_lib::select(&val, path).ok()?.into_iter().next().cloned()
}
```

Crate: `serde_json = "1"`, `jsonpath-lib = "0.3"`.

---

### SQLite FTS5 — Full-Text Search con Índice Invertido

Búsqueda de texto completo tokenizando el contenido y construyendo un **índice invertido**:
cada token apunta a la lista de filas donde aparece. Sin FTS, buscar palabras requiere
`LIKE '%palabra%'` → full scan. Con FTS → lookup O(log n) en el índice.

```
Texto original:
  fila 1: "en el principio creó Dios los cielos"
  fila 2: "Dios es amor y el que permanece en amor"
  fila 3: "el amor de Dios fue manifestado"

Índice invertido (después de tokenizar):
  "dios"      → [fila1(pos:4), fila2(pos:1), fila3(pos:3)]
  "amor"      → [fila2(pos:4), fila2(pos:8), fila3(pos:2)]
  "principio" → [fila1(pos:3)]
  "cielos"    → [fila1(pos:6)]
```

#### Tokenizer

```rust
struct Tokenizer {
    stop_words: HashSet<&'static str>,  // "el", "la", "en", "de", "y"...
}

impl Tokenizer {
    fn tokenize(&self, text: &str) -> Vec<(String, usize)> {  // (token, posición)
        text.to_lowercase()
            .split(|c: char| !c.is_alphanumeric())
            .enumerate()
            .filter(|(_, tok)| !tok.is_empty() && !self.stop_words.contains(tok))
            .map(|(pos, tok)| (stem(tok), pos))  // stemming: "amando" → "am"
            .collect()
    }
}

// Stemming básico en español (sufijos comunes)
fn stem(word: &str) -> String {
    for suffix in &["ando", "iendo", "ción", "mente", "ados", "idos"] {
        if word.ends_with(suffix) {
            return word[..word.len() - suffix.len()].to_string();
        }
    }
    word.to_string()
}
```

#### Estructura del índice invertido

```rust
struct PostingList {
    doc_id:    u64,
    positions: Vec<u16>,   // posiciones del token en el doc (para phrase search)
    tf:        u32,         // term frequency (cuántas veces aparece)
}

struct FtsIndex {
    // token → lista de documentos donde aparece
    index: BTreeMap<String, Vec<PostingList>>,
    // doc_id → longitud del documento (para BM25)
    doc_lengths: HashMap<u64, u32>,
    total_docs: u64,
}
```

#### Ranking BM25 (el mismo que usa SQLite FTS5)

El score decide qué resultados son más relevantes. BM25 es el estándar de la industria.

```rust
fn bm25_score(
    tf: f64,          // cuántas veces aparece el término en este doc
    df: f64,          // en cuántos docs aparece el término
    doc_len: f64,     // longitud del documento
    avg_doc_len: f64, // longitud promedio en la colección
    total_docs: f64,
) -> f64 {
    let k1 = 1.2;  // saturación de term frequency
    let b  = 0.75; // penalización por longitud del doc

    let idf = ((total_docs - df + 0.5) / (df + 0.5) + 1.0).ln();
    let tf_norm = tf * (k1 + 1.0) / (tf + k1 * (1.0 - b + b * doc_len / avg_doc_len));

    idf * tf_norm
}
```

#### SQL — sintaxis FTS

```sql
-- Crear tabla con FTS habilitado
CREATE VIRTUAL TABLE verses_fts USING fts(
  book    TEXT,
  chapter INT,
  verse   INT,
  text    TEXT,           -- columna indexada para FTS
  tokenizer = 'spanish'  -- tokenizer con stop words en español
);

-- Búsqueda simple (OR implícito)
SELECT book, chapter, verse, text
FROM verses_fts
WHERE text MATCH 'amor gracia'
ORDER BY rank;           -- rank = BM25 score

-- Búsqueda de frase exacta
SELECT * FROM verses_fts
WHERE text MATCH '"principio creó"';

-- Búsqueda booleana
SELECT * FROM verses_fts
WHERE text MATCH 'dios AND amor NOT temor';

-- Prefijo (autocompletado)
SELECT * FROM verses_fts
WHERE text MATCH 'amor*';   -- amores, amoroso, amorosa...
```

#### Integración con el motor principal

```rust
enum TableKind {
    Regular(StorageEngine),   // tabla normal con B+ Tree
    Fts(FtsTable),            // tabla virtual con índice invertido
}

struct FtsTable {
    // Datos reales en tabla regular subyacente
    data: StorageEngine,
    // Índice invertido en archivo separado
    index: FtsIndex,          // tabla.fts (serializado con bincode)
}

impl FtsTable {
    fn insert(&mut self, row: &Row) -> Result<()> {
        // 1. Insertar fila en tabla subyacente
        let doc_id = self.data.insert(row)?;
        // 2. Tokenizar columnas FTS y actualizar índice
        for col in &self.fts_columns {
            let text = row.get(col).as_str();
            for (token, pos) in self.tokenizer.tokenize(text) {
                self.index.add_posting(token, doc_id, pos);
            }
        }
        Ok(())
    }

    fn search(&self, query: &FtsQuery) -> Vec<(u64, f64)> {
        // Retorna (doc_id, bm25_score) ordenados por relevancia
        query.terms.iter()
            .flat_map(|term| self.index.lookup(term))
            .fold(HashMap::new(), |mut scores, (doc_id, tf)| {
                *scores.entry(doc_id).or_default() +=
                    bm25_score(tf, self.index.df(term), ...);
                scores
            })
            .into_iter()
            .sorted_by(|a, b| b.1.partial_cmp(&a.1).unwrap())
            .collect()
    }
}
```

#### Archivos en disco para FTS

```
tabla.db     ← datos reales (filas completas)
tabla.wal    ← log de cambios
tabla.idx    ← B+ Tree principal
tabla.fts    ← índice invertido serializado (BTreeMap<String, Vec<PostingList>>)
```

**Ganancia vs LIKE '%palabra%':**
```
LIKE '%amor%' en 1M filas:   full scan → ~3.4s
FTS MATCH 'amor':            índice invertido → ~2ms   (1700x más rápido)
```

Crates: `tantivy = "0.22"` (FTS completo en Rust, como Lucene) o implementación propia
usando `bincode` para serializar el índice.

---

### FoundationDB — Deterministic Simulation Testing

El motor de la BD no sabe si está hablando con disco real o con un simulador.
Se inyectan fallos determinísticos (con semilla) para encontrar bugs de corrupción.

```rust
trait StorageIO: Send + Sync {
    fn read_page(&self, id: u64) -> Result<[u8; 8192]>;
    fn write_page(&self, id: u64, data: &[u8; 8192]) -> Result<()>;
    fn flush(&self) -> Result<()>;
}

// Producción: disco real
struct DiskIO { file: File }
impl StorageIO for DiskIO { ... }

// Tests: simula fallos aleatorios determinísticos
struct FaultInjector {
    inner: DiskIO,
    seed: u64,
    fail_rate: f64,  // 0.01 = 1% de writes fallan
}
impl StorageIO for FaultInjector {
    fn write_page(&self, id: u64, data: &[u8; 8192]) -> Result<()> {
        if self.should_fail(id) {  // determinístico con semilla
            return Err(IoError::new(ErrorKind::Other, "simulated disk failure"));
        }
        self.inner.write_page(id, data)
    }
}

// Test: misma semilla = mismo fallo = reproducible
#[test]
fn test_crash_recovery() {
    let io = FaultInjector::new(seed: 42, fail_rate: 0.05);
    let db = Database::new(io);
    // ... operaciones → alguna falla → crash recovery → verificar consistencia
}
```

---

### HyPer / Umbra — JIT Compilation con LLVM

En vez de interpretar el plan en runtime, compilarlo a código máquina.
Speedup de 5-50x en queries analíticas complejas.

```
Query SQL → Plan lógico → Plan físico → Compilación LLVM JIT → Código máquina → Ejecutar

Plan interpretado:  for row in table { if eval_predicate(row, &plan.filter) { emit(row) } }
                    ← overhead de dispatch en cada fila

Plan compilado:     ; código LLVM IR generado para "WHERE age > 25 AND active = 1"
                    %age = load i32, ptr %row_age
                    %cmp = icmp sgt i32 %age, 25
                    br i1 %cmp, label %check_active, label %skip
                    ← cero overhead de dispatch, inline directo
```

```rust
// inkwell = bindings LLVM para Rust
use inkwell::context::Context;

struct JitCompiler {
    context: Context,
    cache: HashMap<QueryFingerprint, CompiledPlan>,
}

impl JitCompiler {
    fn compile(&mut self, plan: &PhysicalPlan) -> CompiledPlan {
        let module = self.context.create_module("query");
        let builder = self.context.create_builder();
        // Generar LLVM IR para cada operador del plan
        self.emit_scan(&plan.scan, &module, &builder);
        self.emit_filter(&plan.filter, &module, &builder);
        self.emit_project(&plan.project, &module, &builder);
        // JIT compile → función nativa
        module.create_jit_execution_engine(OptimizationLevel::Default)
              .unwrap()
              .into()
    }
}
```

Crate: `inkwell = "0.4"` (requiere LLVM instalado). Fase final del proyecto.

---

## Optimizaciones inspiradas en PostgreSQL (avanzadas)

### Materialized Views

Precomputar el resultado de una query cara y guardarlo como tabla física.
Se refresca explícitamente o de forma incremental.

```sql
CREATE MATERIALIZED VIEW ventas_por_mes AS
SELECT DATE_TRUNC('month', created_at) AS mes, SUM(total) AS total
FROM orders GROUP BY 1;

-- Refrescar manualmente
REFRESH MATERIALIZED VIEW ventas_por_mes;

-- Query sobre la vista (instantáneo, no recalcula)
SELECT * FROM ventas_por_mes WHERE mes > '2025-01-01';
```

```rust
struct MaterializedView {
    name:       String,
    query:      PhysicalPlan,       // query original
    storage:    StorageEngine,      // tabla física con el resultado
    last_refresh: Option<Timestamp>,
}

impl MaterializedView {
    fn refresh(&mut self, db: &Database) -> Result<()> {
        let result = db.execute_plan(&self.query)?;
        self.storage.truncate()?;
        self.storage.bulk_insert(result)?;
        self.last_refresh = Some(Timestamp::now());
        Ok(())
    }
}
```

---

### Window Functions

Funciones que operan sobre una ventana de filas relacionadas sin colapsar el resultado.

```sql
SELECT
  name, dept, salary,
  RANK()        OVER (PARTITION BY dept ORDER BY salary DESC) AS rank_en_dept,
  ROW_NUMBER()  OVER (ORDER BY hire_date)                     AS numero_fila,
  LAG(salary)   OVER (ORDER BY hire_date)                     AS salario_anterior,
  LEAD(salary)  OVER (ORDER BY hire_date)                     AS salario_siguiente,
  SUM(salary)   OVER (PARTITION BY dept)                      AS total_dept,
  AVG(salary)   OVER (PARTITION BY dept ROWS BETWEEN 2 PRECEDING AND CURRENT ROW)
FROM employees;
```

```rust
enum WindowFunc {
    Rank, RowNumber, DenseRank,
    Lag(usize),  Lead(usize),     // offset de filas
    Agg(AggFunc),                 // SUM, AVG, MIN, MAX sobre la ventana
}

struct WindowSpec {
    partition_by: Vec<Expr>,
    order_by:     Vec<(Expr, Order)>,
    frame:        WindowFrame,    // ROWS/RANGE BETWEEN ... AND ...
}

fn eval_window(rows: &[Row], func: &WindowFunc, spec: &WindowSpec) -> Vec<Value> {
    let partitions = group_by_partition(rows, &spec.partition_by);
    partitions.flat_map(|partition| {
        let sorted = sort_by(partition, &spec.order_by);
        apply_window_func(&sorted, func, &spec.frame)
    }).collect()
}
```

---

### Generated / Computed Columns

Columnas cuyo valor se calcula automáticamente a partir de otras columnas.
Se guardan en disco (STORED) o se calculan en cada read (VIRTUAL).

```sql
CREATE TABLE products (
  price     DECIMAL(10,2),
  tax_rate  DECIMAL(4,4),
  total     DECIMAL(10,2) GENERATED ALWAYS AS (price * (1 + tax_rate)) STORED,
  slug      TEXT          GENERATED ALWAYS AS (LOWER(REPLACE(name, ' ', '-'))) VIRTUAL
);

-- No puedes hacer INSERT en columnas GENERATED
INSERT INTO products (name, price, tax_rate) VALUES ('Biblia RV60', 25.00, 0.19);
-- total y slug se calculan solos
```

```rust
enum ColumnKind {
    Regular,
    Generated {
        expr:    Expr,
        storage: GenStorage,
    },
}

enum GenStorage { Stored, Virtual }

fn compute_generated(row: &mut Row, schema: &Schema) {
    for col in schema.columns.iter().filter(|c| matches!(c.kind, Generated { .. })) {
        if let Generated { expr, storage: Stored } = &col.kind {
            row.set(col.idx, expr.eval(row));
        }
    }
}
```

---

### LISTEN / NOTIFY — Pub/Sub nativo

Clientes se suscriben a canales y reciben notificaciones cuando algo cambia.
Sin Redis, sin Kafka — directo en la BD.

```sql
-- Cliente A se suscribe
LISTEN pedidos_nuevos;

-- Cliente B inserta y notifica
INSERT INTO orders (...) VALUES (...);
NOTIFY pedidos_nuevos, '{"id": 99, "total": 150.0}';

-- Cliente A recibe inmediatamente el payload
```

```rust
struct NotifyBus {
    // canal → lista de conexiones suscritas
    channels: DashMap<String, Vec<Sender<String>>>,
}

impl NotifyBus {
    fn listen(&self, channel: &str) -> Receiver<String> {
        let (tx, rx) = tokio::sync::broadcast::channel(128);
        self.channels.entry(channel.to_string()).or_default().push(tx);
        rx
    }

    fn notify(&self, channel: &str, payload: &str) {
        if let Some(subs) = self.channels.get(channel) {
            for tx in subs.iter() {
                let _ = tx.send(payload.to_string());
            }
        }
    }
}
```

---

### Non-blocking Schema Changes (estilo PlanetScale / gh-ost)

`ALTER TABLE` tradicional lockea la tabla entera — puede tardar horas en tablas grandes.
La técnica shadow table hace el cambio sin bloquear reads ni writes.

```
Proceso:
  1. Crear tabla nueva con el esquema modificado  (tabla_shadow)
  2. Copiar datos en batches pequeños (sin lock)
  3. Capturar cambios del WAL durante la copia (CDC)
  4. Aplicar los cambios capturados a tabla_shadow
  5. Swap atómico: renombrar tabla → tabla_shadow  (microsegundos)
  6. Drop tabla vieja
```

```rust
async fn alter_table_nonblocking(
    db: &Database,
    table: &str,
    new_schema: Schema,
) -> Result<()> {
    // 1. Crear shadow table
    let shadow = format!("_{table}_shadow");
    db.create_table(&shadow, &new_schema).await?;

    // 2. Copiar en batches de 1000 filas (sin lock)
    let mut cursor = 0u64;
    loop {
        let batch = db.scan_batch(table, cursor, 1000).await?;
        if batch.is_empty() { break; }
        db.bulk_insert(&shadow, transform_rows(&batch, &new_schema)).await?;
        cursor = batch.last().unwrap().pk;
    }

    // 3. Aplicar WAL delta (cambios durante la copia)
    db.apply_wal_delta(table, &shadow, from_lsn).await?;

    // 4. Swap atómico
    db.rename_table(table, &format!("_{table}_old")).await?;
    db.rename_table(&shadow, table).await?;
    db.drop_table(&format!("_{table}_old")).await?;
    Ok(())
}
```

---

### Covering Indexes

El índice incluye columnas adicionales para que la query nunca toque la tabla principal.

```sql
-- Sin covering index:
CREATE INDEX idx_role ON users(role_id);
-- SELECT name, email FROM users WHERE role_id = 5
-- → busca role_id en índice → obtiene RID → va a tabla → lee name, email  (2 I/Os)

-- Con covering index:
CREATE INDEX idx_role_covering ON users(role_id) INCLUDE (name, email);
-- → busca role_id en índice → name y email YA están en el índice  (1 I/O)
```

```rust
struct IndexDef {
    key_columns:     Vec<usize>,   // columnas de búsqueda
    include_columns: Vec<usize>,   // columnas extra almacenadas en las hojas
    predicate:       Option<Expr>, // para partial index
}

// En las hojas del B+ Tree, guardar también los include_columns
struct BTreeLeaf {
    keys:     Vec<KeyType>,
    rids:     Vec<RecordId>,
    included: Vec<Vec<Value>>,   // valores extra (covering)
}
```

---

## Optimizaciones inspiradas en TimescaleDB

### Table Partitioning automático

Dividir tablas grandes en particiones físicas separadas.
`DELETE` de una partición completa es instantáneo (drop de archivo).

```sql
-- Partición por rango de fecha
CREATE TABLE logs (
  created_at TIMESTAMP,
  level      TEXT,
  message    TEXT
) PARTITION BY RANGE (created_at);

CREATE TABLE logs_2024 PARTITION OF logs
  FOR VALUES FROM ('2024-01-01') TO ('2025-01-01');
CREATE TABLE logs_2025 PARTITION OF logs
  FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');

-- Partition pruning: solo lee logs_2025
SELECT * FROM logs WHERE created_at > '2025-06-01';

-- Drop instantáneo sin borrar fila por fila
DROP TABLE logs_2024;

-- Partición por hash (distribución uniforme)
CREATE TABLE users (...) PARTITION BY HASH (id);
CREATE TABLE users_0 PARTITION OF users FOR VALUES WITH (modulus 4, remainder 0);
CREATE TABLE users_1 PARTITION OF users FOR VALUES WITH (modulus 4, remainder 1);
```

```rust
enum PartitionStrategy {
    Range { column: usize, ranges: Vec<(Value, Value, String)> }, // (min, max, tabla)
    Hash  { column: usize, modulus: u32, partitions: Vec<String> },
    List  { column: usize, values: HashMap<Value, String> },
}

fn route_row(row: &Row, strategy: &PartitionStrategy) -> &str {
    match strategy {
        Range { column, ranges, .. } => {
            let val = row.get(*column);
            ranges.iter().find(|(min, max, _)| val >= min && val < max)
                  .map(|(_, _, name)| name.as_str()).unwrap_or("default")
        }
        Hash { column, modulus, partitions } => {
            let hash = row.get(*column).hash() % *modulus as u64;
            &partitions[hash as usize]
        }
        List { column, values } => values.get(row.get(*column)).map(String::as_str).unwrap()
    }
}
```

---

### Compresión automática de datos históricos

Datos antiguos se comprimen en columnar format — 10-20x menos espacio.

```sql
-- Comprimir particiones con más de 30 días
ALTER TABLE logs SET (compress_after = '30 days');

-- Verificar estado
SELECT chunk_name, compressed_total_bytes, uncompressed_total_bytes
FROM chunk_compression_stats('logs');
```

```rust
struct ChunkCompressor;

impl ChunkCompressor {
    fn compress_chunk(chunk: &StorageEngine) -> Result<CompressedChunk> {
        // Convertir row-store a columnar + LZ4
        let columns = chunk.to_columnar();
        let compressed = columns.iter()
            .map(|col| lz4_flex::compress_prepend_size(col))
            .collect();
        Ok(CompressedChunk { compressed, original_rows: chunk.len() })
    }
}
```

---

### Continuous Aggregates (vistas materializadas que se auto-actualizan)

```sql
CREATE MATERIALIZED VIEW metrics_hourly
WITH (timescaledb.continuous) AS
SELECT
  TIME_BUCKET('1 hour', time) AS bucket,
  AVG(value)                  AS avg_val,
  MAX(value)                  AS max_val
FROM metrics
GROUP BY bucket;

-- Solo recomputa el delta nuevo, no toda la tabla
REFRESH MATERIALIZED VIEW metrics_hourly
  WITH (start => NOW() - INTERVAL '2 hours', end => NOW());
```

---

## Optimizaciones inspiradas en Redis

### TTL por fila

Filas con fecha de expiración automática. Sin cron, sin DELETE manual.

```sql
INSERT INTO sessions (id, user_id, token)
VALUES (1, 99, 'abc123')
WITH TTL 3600;  -- expira en 1 hora

INSERT INTO cache_entries (key, value)
VALUES ('config', '{"theme":"dark"}')
WITH TTL 86400;  -- expira en 24 horas
```

```rust
struct RowHeader {
    txn_id_created: u64,
    txn_id_deleted: u64,
    row_version:    u32,
    expires_at:     Option<u64>,   // Unix timestamp, None = no expira
}

// Background task en Tokio: barrer filas expiradas
async fn ttl_reaper(engine: Arc<Engine>) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        let now = unix_timestamp();
        engine.delete_expired_rows(now).await;
    }
}

fn is_visible(row: &RowHeader, now: u64) -> bool {
    row.txn_id_deleted == 0
        && row.expires_at.map(|exp| exp > now).unwrap_or(true)
}
```

---

### LRU Eviction para modo in-memory

Cuando la BD en memoria alcanza el límite de RAM, expulsa las páginas menos usadas.

```rust
struct LruPageCache {
    capacity: usize,
    pages:    LinkedHashMap<u64, [u8; 8192]>,  // LRU order
}

impl LruPageCache {
    fn get(&mut self, page_id: u64) -> Option<&[u8; 8192]> {
        self.pages.get_refresh(&page_id)  // mueve al frente (más reciente)
    }

    fn insert(&mut self, page_id: u64, page: [u8; 8192]) {
        if self.pages.len() >= self.capacity {
            self.pages.pop_back();  // evict LRU
        }
        self.pages.insert(page_id, page);
    }
}
```

---

## Optimizaciones inspiradas en MongoDB

### Change Streams — CDC (Change Data Capture)

Escuchar cambios en la BD en tiempo real. Sin polling. Basado en el WAL.

```rust
// La app se suscribe a cambios de una tabla
let mut stream = db.watch("users").await?;

while let Some(event) = stream.next().await {
    match event {
        ChangeEvent::Insert { table, row } => {
            println!("Nuevo usuario: {:?}", row);
            send_welcome_email(&row).await;
        }
        ChangeEvent::Update { table, old, new } => {
            if old.get("role_id") != new.get("role_id") {
                revoke_permissions(&old).await;
                grant_permissions(&new).await;
            }
        }
        ChangeEvent::Delete { table, id } => {
            cleanup_user_data(id).await;
        }
    }
}
```

```rust
// Internamente: leer el WAL y emitir eventos
struct ChangeStream {
    table:   String,
    wal_pos: u64,
    tx:      broadcast::Sender<ChangeEvent>,
}

impl ChangeStream {
    async fn tail_wal(&mut self, wal: &WalReader) {
        loop {
            if let Some(entry) = wal.read_from(self.wal_pos).await {
                if entry.table == self.table {
                    let _ = self.tx.send(entry.into_event());
                }
                self.wal_pos = entry.lsn + 1;
            }
        }
    }
}
```

---

## Optimizaciones inspiradas en DoltDB

### Git para datos — Branching y Versioning

La BD tiene historia completa. Puedes hacer branches, commits, merge y diff de datos.

```sql
-- Ver historia
SELECT * FROM dolt_log;

-- Crear branch para desarrollo
CALL dolt_branch('feature/nueva-tabla');
CALL dolt_checkout('feature/nueva-tabla');

-- Hacer cambios en el branch
CREATE TABLE nueva (id INT PRIMARY KEY, data TEXT);
INSERT INTO nueva VALUES (1, 'prueba');

-- Commit
CALL dolt_commit('-m', 'agregar tabla nueva');

-- Merge a main
CALL dolt_checkout('main');
CALL dolt_merge('feature/nueva-tabla');

-- Diff entre branches
SELECT * FROM dolt_diff_nueva WHERE from_commit = 'main' AND to_commit = 'HEAD';

-- Rollback a commit anterior
CALL dolt_reset('--hard', 'abc123def');
```

```rust
struct DoltCommit {
    hash:      [u8; 20],          // SHA1 del estado
    parent:    Option<[u8; 20]>,
    message:   String,
    timestamp: u64,
    // snapshot del root de cada B+ Tree en este commit
    table_roots: HashMap<String, u64>,
}

struct VersionedDatabase {
    engine:   StorageEngine,
    commits:  Vec<DoltCommit>,     // historia
    branches: HashMap<String, [u8; 20]>,  // branch → commit hash
    head:     String,              // branch actual
}

impl VersionedDatabase {
    fn commit(&mut self, message: &str) -> [u8; 20] {
        // Snapshot de los roots actuales de todos los B+ Trees
        let roots = self.engine.snapshot_roots();
        let commit = DoltCommit::new(roots, message, self.current_head());
        let hash = commit.hash();
        self.commits.push(commit);
        self.branches.insert(self.head.clone(), hash);
        hash
    }

    fn checkout(&mut self, branch: &str) -> Result<()> {
        let hash = self.branches[branch];
        let commit = self.find_commit(hash);
        // Restaurar roots de los B+ Trees a ese commit
        self.engine.restore_roots(&commit.table_roots)
    }
}
```

---

## Apache Arrow — Formato columnar para resultados

En vez de devolver filas, devolver columnas. Cero serialización con Python/pandas/Spark.

```rust
use arrow::array::{Int32Array, StringArray};
use arrow::record_batch::RecordBatch;

fn query_to_arrow(result: QueryResult) -> RecordBatch {
    // Convertir resultado row-store a columnar Arrow
    let ids:   Int32Array  = result.rows.iter().map(|r| r.get_i32("id")).collect();
    let names: StringArray = result.rows.iter().map(|r| r.get_str("name")).collect();

    RecordBatch::try_new(
        Arc::new(schema),
        vec![Arc::new(ids), Arc::new(names)],
    ).unwrap()
}

// El cliente Python recibe Arrow directamente — cero copia
// import pyarrow as pa
// df = pa.RecordBatch  → pandas.DataFrame sin serialización
```

Crate: `arrow = "52"` (Apache Arrow oficial para Rust).

---

## Lógica del servidor — Alternativas modernas a Stored Procedures

Tres capas en orden de complejidad. Se implementan en ese orden.

---

### Capa 1: SQL UDFs + Triggers

La base. Funciones y triggers escritos en SQL puro.

```sql
-- UDF escalar: recibe valores, retorna un valor
CREATE FUNCTION descuento(precio DECIMAL, categoria INT) RETURNS DECIMAL AS $$
  CASE
    WHEN categoria = 1 THEN precio * 0.85
    WHEN categoria = 2 THEN precio * 0.90
    ELSE precio
  END
$$;

-- Usar en cualquier query
SELECT nombre, precio, descuento(precio, categoria_id) AS precio_final
FROM products;

-- UDF de tabla: retorna múltiples filas
CREATE FUNCTION ventas_del_mes(mes INT, anio INT) RETURNS TABLE AS $$
  SELECT * FROM orders
  WHERE MONTH(created_at) = mes AND YEAR(created_at) = anio
$$;

SELECT * FROM ventas_del_mes(3, 2026);

-- Trigger BEFORE: validar antes de insertar
CREATE TRIGGER validar_stock
  BEFORE INSERT ON order_items
  FOR EACH ROW
  BEGIN
    IF (SELECT stock FROM products WHERE id = NEW.product_id) < NEW.quantity THEN
      SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT = 'stock insuficiente';
    END IF;
  END;

-- Trigger AFTER: auditoría automática
CREATE TRIGGER audit_roles
  AFTER UPDATE ON users
  FOR EACH ROW
  WHEN (OLD.role_id != NEW.role_id)
  BEGIN
    INSERT INTO audit_log (user_id, old_role, new_role, changed_at)
    VALUES (NEW.id, OLD.role_id, NEW.role_id, NOW());
  END;
```

```rust
// Representación interna de UDF SQL
struct SqlFunction {
    name:        String,
    params:      Vec<(String, DataType)>,
    return_type: ReturnType,
    body:        Expr,             // AST del cuerpo
}

enum ReturnType {
    Scalar(DataType),
    Table(Schema),
}

// Representación interna de Trigger
struct Trigger {
    name:       String,
    table:      String,
    timing:     TriggerTiming,    // Before | After
    event:      TriggerEvent,     // Insert | Update | Delete
    for_each:   ForEach,          // Row | Statement
    when:       Option<Expr>,     // condición WHEN opcional
    body:       Vec<Statement>,   // sentencias SQL del cuerpo
}

enum TriggerTiming { Before, After }
enum TriggerEvent  { Insert, Update, Delete }
```

---

### Capa 2: Lua Scripting — scripts atómicos (estilo Redis EVAL)

Scripts que corren dentro de una transacción atómica. Pueden hacer queries,
procesar resultados en Lua y ejecutar más queries. Sin race conditions.

```sql
-- EVAL ejecuta el script Lua en una transacción atómica
EVAL '
  -- KEYS: tablas/ids involucrados
  -- ARGS: parámetros del script

  local from_id = tonumber(KEYS[1])
  local to_id   = tonumber(KEYS[2])
  local amount  = tonumber(ARGS[1])

  -- query() retorna tabla Lua con los resultados
  local from_acc = query("SELECT balance FROM accounts WHERE id = ?", from_id)

  if from_acc[1].balance < amount then
    error("saldo insuficiente: " .. from_acc[1].balance)
  end

  execute("UPDATE accounts SET balance = balance - ? WHERE id = ?", amount, from_id)
  execute("UPDATE accounts SET balance = balance + ? WHERE id = ?", amount, to_id)

  -- log de auditoría dentro del mismo script
  execute("INSERT INTO transfers (from_id, to_id, amount) VALUES (?, ?, ?)",
          from_id, to_id, amount)

  return { status = "ok", new_balance = from_acc[1].balance - amount }
' KEYS [1, 2] ARGS [100.0]
```

```rust
use mlua::prelude::*;

struct LuaRuntime {
    lua: Lua,
}

impl LuaRuntime {
    fn eval(&self, script: &str, keys: Vec<Value>, args: Vec<Value>,
            db: Arc<Database>) -> Result<LuaValue> {

        let lua = &self.lua;

        // Exponer query() y execute() al script Lua
        let db_query = db.clone();
        lua.globals().set("query", lua.create_function(move |_, (sql, params): (String, LuaTable)| {
            let result = db_query.execute_raw(&sql, &lua_to_values(params))?;
            Ok(result_to_lua(result))
        })?)?;

        let db_exec = db.clone();
        lua.globals().set("execute", lua.create_function(move |_, (sql, params): (String, LuaTable)| {
            db_exec.execute_raw(&sql, &lua_to_values(params))?;
            Ok(())
        })?)?;

        // Correr dentro de una transacción
        db.begin_transaction()?;
        let result = lua.load(script).eval::<LuaValue>();
        match result {
            Ok(val) => { db.commit()?; Ok(val) }
            Err(e)  => { db.rollback()?; Err(e.into()) }
        }
    }
}
```

**Casos de uso perfectos para Lua:**
- Transferencias bancarias atómicas
- Lógica de puntos/rewards
- Rate limiting en la BD
- Cualquier operación read-then-write sin race conditions

---

### Capa 3: WASM Plugins — lógica en cualquier lenguaje (estilo Cloudflare D1 / SingleStore)

El diferenciador real. Escribes una función en **Rust, Python, Go, C++ o JS**,
la compilas a WebAssembly, y la cargas en la BD. Corre con sandboxing completo.

```sql
-- Cargar plugin WASM desde archivo
CREATE FUNCTION calcular_riesgo
  LANGUAGE wasm
  FROM FILE 'plugins/riesgo_credito.wasm'
  RETURNS FLOAT;

-- Cargar plugin WASM inline (base64)
CREATE FUNCTION normalizar_texto
  LANGUAGE wasm
  FROM BASE64 'AGFzbQEAAAA...'
  RETURNS TEXT;

-- Usar como función SQL normal
SELECT cliente_id,
       calcular_riesgo(historial_pagos, deuda_total, ingresos) AS score
FROM clientes
WHERE calcular_riesgo(historial_pagos, deuda_total, ingresos) > 0.7;

-- Como trigger con lógica WASM
CREATE TRIGGER enriquecer_datos
  BEFORE INSERT ON contacts
  FOR EACH ROW
  EXECUTE FUNCTION normalizar_texto(NEW.nombre, NEW.email);
```

**Plugin escrito en Rust:**
```rust
// plugins/riesgo/src/lib.rs
// cargo build --target wasm32-unknown-unknown --release

#[no_mangle]
pub extern "C" fn calcular_riesgo(
    historial: f64,   // 0.0 a 1.0 (porcentaje pagos a tiempo)
    deuda:     f64,   // deuda total en USD
    ingresos:  f64,   // ingresos mensuales en USD
) -> f64 {
    let ratio_deuda = deuda / (ingresos * 12.0);
    let score = historial * 0.5 + (1.0 - ratio_deuda.min(1.0)) * 0.5;
    1.0 - score  // riesgo = inverso del score
}
```

**Plugin escrito en Python (compila a WASM con py2wasm):**
```python
# plugins/sentiment/sentiment.py
def analizar_sentimiento(texto: str) -> float:
    positivas = ["bueno", "excelente", "amor", "feliz", "gracias"]
    negativas = ["malo", "terrible", "odio", "triste", "problema"]
    score = sum(1 for p in positivas if p in texto.lower())
    score -= sum(1 for n in negativas if n in texto.lower())
    return max(-1.0, min(1.0, score / 5.0))
```

**Motor WASM interno:**
```rust
use wasmtime::{Engine, Module, Store, Instance, Val};

struct WasmPlugin {
    module:   Module,
    metadata: PluginMeta,
}

struct WasmRuntime {
    engine:  Engine,
    plugins: HashMap<String, WasmPlugin>,  // nombre → módulo compilado
}

impl WasmRuntime {
    fn load_plugin(&mut self, name: &str, wasm_bytes: &[u8]) -> Result<()> {
        // Compilar WASM a código nativo (una sola vez al cargar)
        let module = Module::new(&self.engine, wasm_bytes)?;
        self.plugins.insert(name.to_string(), WasmPlugin { module, .. });
        Ok(())
    }

    fn call(&self, func_name: &str, args: &[Val]) -> Result<Vec<Val>> {
        let plugin = &self.plugins[func_name];
        let mut store = Store::new(&self.engine, ());

        // Límites de seguridad: sandbox real
        store.limiter(|_| {
            StoreLimitsBuilder::new()
                .memory_size(16 * 1024 * 1024)  // max 16MB por llamada
                .instances(1)
                .build()
        });

        let instance = Instance::new(&mut store, &plugin.module, &[])?;
        let func = instance.get_func(&mut store, func_name)
            .ok_or("función no encontrada")?;

        let mut results = vec![Val::I32(0); func.ty(&store).results().len()];
        func.call(&mut store, args, &mut results)?;
        Ok(results)
    }
}
```

**Seguridad del sandbox WASM:**
```
✓ Sin acceso al filesystem (a menos que se lo des explícitamente)
✓ Sin acceso a la red
✓ Memoria aislada (no puede leer memoria de la BD)
✓ Timeout configurable (mata el plugin si tarda demasiado)
✓ Límite de memoria por llamada
✓ Determinístico: mismo input = mismo output siempre
```

---

### Resumen de las tres capas

```
┌─────────────────────────────────────────────────────────────┐
│                    LÓGICA DEL SERVIDOR                      │
│                                                             │
│  Capa 1: SQL UDFs + Triggers                               │
│    → lógica simple, sin dependencias externas              │
│    → CREATE FUNCTION / CREATE TRIGGER en SQL puro          │
│                                                             │
│  Capa 2: Lua Scripts (EVAL)                                │
│    → lógica compleja, atómica, read-then-write seguro      │
│    → acceso a query() y execute() dentro del script        │
│                                                             │
│  Capa 3: WASM Plugins                                      │
│    → lógica en cualquier lenguaje (Rust/Python/Go/JS)      │
│    → sandboxed, compilado a nativo, máxima velocidad       │
│    → CREATE FUNCTION ... LANGUAGE wasm FROM FILE '...'     │
└─────────────────────────────────────────────────────────────┘
```

| | SQL UDFs | Lua EVAL | WASM |
|---|---|---|---|
| Lenguaje | SQL | Lua | Cualquiera |
| Velocidad | Alta | Media | Muy alta |
| Atomicidad | En trigger | Siempre | Configurable |
| Dependencias ext. | No | No | No (sandbox) |
| Ideal para | Transformaciones | Transacciones complejas | ML, NLP, lógica de negocio |

---

## Seguridad — Usuarios, Roles y Permisos (RBAC)

### Usuarios y Roles

```sql
-- Gestión de usuarios
CREATE USER ana WITH PASSWORD 'segura123';
CREATE USER bot WITH PASSWORD 'bot456' CONNECTION LIMIT 5;
DROP USER ana;
ALTER USER ana WITH PASSWORD 'nueva123';

-- Roles (grupos de permisos reutilizables)
CREATE ROLE readonly;
CREATE ROLE editor;
CREATE ROLE admin;

-- Asignar rol a usuario (herencia de permisos)
GRANT readonly TO ana;
GRANT editor   TO ana;   -- ana hereda ambos roles
REVOKE readonly FROM ana;
```

### Grants por tabla y columna

```sql
-- Permisos de tabla
GRANT SELECT                        ON products TO readonly;
GRANT SELECT, INSERT, UPDATE        ON orders   TO editor;
GRANT ALL PRIVILEGES                ON ALL TABLES TO admin;
REVOKE DELETE                       ON orders   FROM editor;

-- Permisos por columna (ana puede ver nombre y email, pero NO password)
GRANT SELECT (id, nombre, email)    ON users TO ana;
REVOKE SELECT (password_hash)       ON users FROM PUBLIC;

-- Permisos por esquema
GRANT USAGE ON SCHEMA ventas TO ana;
GRANT ALL   ON ALL TABLES IN SCHEMA ventas TO admin;
```

### Autenticación — Argon2id + Scram-SHA-256

```rust
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::password_hash::{SaltString, rand_core::OsRng};

fn hash_password(password: &str) -> String {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .unwrap()
        .to_string()
    // $argon2id$v=19$m=65536,t=3,p=4$...
}

fn verify_password(password: &str, hash: &str) -> bool {
    let parsed = PasswordHash::new(hash).unwrap();
    Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok()
}
```

Wire protocol: **Scram-SHA-256** (estándar PostgreSQL moderno, nunca envía el password en claro).

### TLS — conexiones cifradas

```toml
# dbyo.toml
[tls]
enabled  = true
cert     = "/etc/dbyo/server.crt"
key      = "/etc/dbyo/server.key"
min_version = "TLS1.3"
require_client_cert = false   # true para mTLS
```

```rust
use tokio_rustls::TlsAcceptor;

async fn accept_connection(stream: TcpStream, tls: &TlsAcceptor) {
    let tls_stream = tls.accept(stream).await.unwrap();
    handle_client(tls_stream).await;
}
```

Crate: `tokio-rustls = "0.26"`, `rustls = "0.23"`.

---

### Row-Level Security (RLS)

Filtros automáticos por usuario. El cliente no puede saltárselos — viven en el motor.

```sql
-- Habilitar RLS en la tabla
ALTER TABLE orders ENABLE ROW LEVEL SECURITY;

-- Política: cada usuario solo ve sus propios pedidos
CREATE POLICY mis_pedidos ON orders
  FOR SELECT
  USING (user_id = CURRENT_USER_ID());

-- Política para admins: ven todo
CREATE POLICY admin_todo ON orders
  FOR ALL
  TO admin
  USING (true);

-- Ana hace SELECT * FROM orders → el motor agrega automáticamente:
-- WHERE user_id = (id de Ana)   ← invisible para Ana
```

```rust
fn apply_rls(plan: &mut PhysicalPlan, user: &User, table: &str) {
    let policies = user.applicable_policies(table);
    if !policies.is_empty() {
        let rls_filter = policies.iter()
            .map(|p| p.using_expr.clone())
            .reduce(|a, b| Expr::Or(Box::new(a), Box::new(b)))
            .unwrap();
        plan.prepend_filter(rls_filter);
    }
}
```

---

## Alta Disponibilidad — Replicación y PITR

### Streaming Replication (Primary → Replicas)

```
Primary ──WAL stream──► Replica 1  (sincrónica  — confirma antes de commit)
        ──WAL stream──► Replica 2  (asincrónica — no bloquea el primary)

Clientes:
  Writes  → Primary  (siempre)
  Reads   → Replica  (load balancing, lecturas eventualmente consistentes)
```

```toml
# primary dbyo.toml
[replication]
role = "primary"
replicas = ["replica1:5433", "replica2:5433"]
sync_replicas = 1   # al menos 1 réplica sincrónica antes de confirmar commit
wal_retention = "7d"

# replica dbyo.toml
[replication]
role    = "replica"
primary = "primary:3306"
```

```rust
struct WalSender {
    replica_addr: SocketAddr,
    last_lsn:     AtomicU64,
}

impl WalSender {
    async fn stream(&self, wal: &WalReader) {
        let mut stream = TcpStream::connect(self.replica_addr).await.unwrap();
        loop {
            let entry = wal.read_from(self.last_lsn.load(Ordering::Relaxed)).await;
            stream.write_all(&entry.serialize()).await.unwrap();
            self.last_lsn.fetch_add(1, Ordering::Relaxed);
        }
    }
}
```

### Point-in-Time Recovery (PITR)

```sql
-- Restaurar al estado exacto de hace 2 horas
RESTORE DATABASE mydb
  FROM BACKUP '/backups/mydb_2026-03-21.base'
  TO TIMESTAMP '2026-03-21 10:30:00';

-- El motor:
-- 1. Restaura el backup base
-- 2. Reproduce el WAL hasta el timestamp exacto
-- 3. Para justo antes de la primera operación post-timestamp
```

```rust
async fn pitr_restore(backup_path: &str, target_time: u64) -> Result<()> {
    // 1. Restaurar backup base (copia del .db en el timestamp del backup)
    restore_base_backup(backup_path).await?;

    // 2. Reproducir WAL hasta target_time
    let wal = WalReader::open("mydb.wal")?;
    for entry in wal.iter() {
        if entry.timestamp > target_time { break; }
        entry.apply(&mut engine).await?;
    }
    Ok(())
}
```

---

## Mantenimiento Automático

### Vacuum — recuperar espacio de filas muertas MVCC

```sql
-- Manual
VACUUM orders;          -- libera páginas de filas con txn_id_deleted > 0
VACUUM ANALYZE orders;  -- + recalcula estadísticas para el query planner
VACUUM FULL orders;     -- compacta completamente (lockea la tabla)

-- Ver cuánto espacio muerto hay
SELECT table_name, dead_rows, live_rows, last_vacuum
FROM db_stat_tables;
```

```rust
// Auto-vacuum en background (Tokio task)
async fn autovacuum(engine: Arc<Engine>) {
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    loop {
        interval.tick().await;
        for table in engine.tables_needing_vacuum().await {
            // vacuum si dead_rows > 20% del total
            if table.dead_ratio() > 0.20 {
                engine.vacuum(&table.name).await.ok();
            }
        }
    }
}
```

### Deadlock Detection

```rust
// Grafo de espera: Tx → lista de Txs que espera
struct WaitGraph {
    edges: HashMap<TxnId, Vec<TxnId>>,
}

impl WaitGraph {
    // Detectar ciclo con DFS
    fn find_deadlock(&self) -> Option<Vec<TxnId>> {
        let mut visited = HashSet::new();
        let mut path = Vec::new();
        for &start in self.edges.keys() {
            if self.dfs(start, &mut visited, &mut path) {
                return Some(path); // ciclo = deadlock
            }
        }
        None
    }

    fn resolve(&self, cycle: Vec<TxnId>, engine: &Engine) {
        // Matar la transacción más joven (menor trabajo perdido)
        let victim = cycle.iter().max_by_key(|&&id| engine.txn_start_time(id)).unwrap();
        engine.rollback(*victim);
    }
}

// Correr detector cada 100ms
async fn deadlock_detector(engine: Arc<Engine>) {
    let mut interval = tokio::time::interval(Duration::from_millis(100));
    loop {
        interval.tick().await;
        let graph = engine.build_wait_graph();
        if let Some(cycle) = graph.find_deadlock() {
            graph.resolve(cycle, &engine);
        }
    }
}
```

---

## Observabilidad

### pg_stat_statements — estadísticas de queries

```sql
-- Las N queries más lentas
SELECT query, calls, total_time_ms,
       total_time_ms / calls AS avg_ms,
       rows_returned,
       cache_hits, cache_misses
FROM db_stat_statements
ORDER BY total_time_ms DESC
LIMIT 10;

-- Queries más frecuentes
SELECT query, calls FROM db_stat_statements ORDER BY calls DESC LIMIT 5;

-- Resetear estadísticas
SELECT db_stat_reset();
```

```rust
struct QueryStat {
    query_fingerprint: u64,       // hash normalizado del SQL
    query_sample:      String,    // ejemplo de query real
    calls:             u64,
    total_time_us:     u64,
    rows_returned:     u64,
    cache_hits:        u64,
    cache_misses:      u64,
}

struct StatCollector {
    stats: DashMap<u64, QueryStat>,  // concurrent hashmap
}

impl StatCollector {
    fn record(&self, sql: &str, duration: Duration, rows: u64) {
        let fp = fingerprint(sql);  // normaliza: quita literales, normaliza espacios
        self.stats.entry(fp).and_modify(|s| {
            s.calls += 1;
            s.total_time_us += duration.as_micros() as u64;
            s.rows_returned += rows;
        }).or_insert(QueryStat::new(sql, duration, rows));
    }
}
```

### Slow Query Log

```toml
[logging]
slow_query_log       = true
slow_query_threshold = "100ms"
log_file             = "/var/log/dbyo/slow.log"
log_format           = "json"   # o "text"
```

```json
{
  "timestamp": "2026-03-21T10:30:00Z",
  "duration_ms": 342,
  "user": "ana",
  "database": "myapp",
  "query": "SELECT * FROM orders JOIN users ON ...",
  "rows_examined": 1000000,
  "rows_returned": 3,
  "plan": "SeqScan → HashJoin (missing index on orders.user_id)"
}
```

### Statement Timeout

```sql
-- Global
SET statement_timeout = '5s';

-- Por sesión
SET SESSION statement_timeout = '30s';

-- Por usuario
ALTER USER bot SET statement_timeout = '2s';
```

---

## Tipos adicionales

### Views regulares

```sql
CREATE VIEW usuarios_activos AS
  SELECT id, nombre, email, created_at
  FROM users WHERE active = true;

-- Usar como tabla normal
SELECT * FROM usuarios_activos WHERE created_at > '2026-01-01';

-- View actualizable (el motor traduce el INSERT/UPDATE a la tabla base)
CREATE VIEW mis_pedidos AS
  SELECT * FROM orders WHERE user_id = CURRENT_USER_ID();

INSERT INTO mis_pedidos (producto_id, cantidad) VALUES (5, 2);
-- → INSERT INTO orders (user_id, producto_id, cantidad) VALUES (CURRENT_USER_ID(), 5, 2)
```

### Sequences

```sql
CREATE SEQUENCE order_num_seq
  START  1000
  INCREMENT 1
  MINVALUE 1000
  MAXVALUE 9999999
  CYCLE;           -- vuelve a 1000 al llegar al máximo

SELECT NEXTVAL('order_num_seq');  -- 1000, 1001, 1002 ...
SELECT CURRVAL('order_num_seq');  -- valor actual en esta sesión
SELECT SETVAL('order_num_seq', 5000);

-- Usar en tabla
CREATE TABLE orders (
  numero INT DEFAULT NEXTVAL('order_num_seq'),
  ...
);
```

```rust
struct Sequence {
    current:   AtomicI64,
    increment: i64,
    min:       i64,
    max:       i64,
    cycle:     bool,
}

impl Sequence {
    fn next(&self) -> Result<i64> {
        let val = self.current.fetch_add(self.increment, Ordering::SeqCst);
        if val > self.max {
            if self.cycle { self.current.store(self.min, Ordering::SeqCst); Ok(self.min) }
            else { Err(DbError::SequenceExhausted) }
        } else { Ok(val) }
    }
}
```

### ENUMs

```sql
CREATE TYPE estado_pedido AS ENUM ('pendiente', 'procesando', 'enviado', 'entregado', 'cancelado');
CREATE TYPE prioridad     AS ENUM ('baja', 'media', 'alta', 'critica');

CREATE TABLE orders (
  id       INT PRIMARY KEY,
  estado   estado_pedido DEFAULT 'pendiente',
  prioridad prioridad    DEFAULT 'media'
);

-- Validación automática en INSERT/UPDATE
INSERT INTO orders (estado) VALUES ('inventado');  -- ERROR: valor inválido para enum estado_pedido

-- Ordenamiento semántico (mantiene el orden de definición)
SELECT * FROM orders ORDER BY prioridad;  -- baja → media → alta → critica
```

### Arrays

```sql
CREATE TABLE posts (
  id       INT PRIMARY KEY,
  titulo   TEXT,
  tags     TEXT[],
  scores   FLOAT[],
  metadata JSON
);

INSERT INTO posts VALUES (1, 'Rust DB', ARRAY['rust', 'database', 'wasm'], ARRAY[9.5, 8.0]);

-- Buscar en arrays
SELECT * FROM posts WHERE 'rust' = ANY(tags);
SELECT * FROM posts WHERE tags @> ARRAY['rust', 'wasm'];  -- contiene ambos

-- Funciones de array
SELECT array_length(tags), array_append(tags, 'nuevo'), tags[1]
FROM posts;
```

---

## Importación y Exportación de Datos

### CSV

```sql
-- Importar CSV → tabla (con detección automática de tipos)
COPY users FROM '/data/usuarios.csv'
  WITH (
    FORMAT csv,
    HEADER true,
    DELIMITER ',',
    NULL 'NULL',
    ENCODING 'UTF-8'
  );

-- Importar con mapeo de columnas
COPY users (nombre, email, created_at)
FROM '/data/usuarios_parcial.csv'
WITH (FORMAT csv, HEADER true);

-- Exportar tabla → CSV
COPY users TO '/exports/usuarios.csv'
WITH (FORMAT csv, HEADER true, DELIMITER ',');

-- Exportar resultado de query → CSV
COPY (SELECT id, nombre FROM users WHERE active = true)
TO '/exports/activos.csv'
WITH (FORMAT csv, HEADER true);
```

```rust
use csv::ReaderBuilder;

async fn import_csv(path: &str, table: &str, opts: CopyOptions, db: &Database) -> Result<u64> {
    let mut reader = ReaderBuilder::new()
        .has_headers(opts.header)
        .delimiter(opts.delimiter as u8)
        .from_path(path)?;

    let schema = db.table_schema(table)?;
    let mut rows_imported = 0u64;

    // Importar en batches de 1000 filas para eficiencia
    let mut batch = Vec::with_capacity(1000);
    for record in reader.records() {
        let record = record?;
        let row = schema.parse_csv_record(&record, &opts)?;
        batch.push(row);

        if batch.len() == 1000 {
            db.bulk_insert(table, std::mem::take(&mut batch)).await?;
            rows_imported += 1000;
        }
    }
    if !batch.is_empty() {
        rows_imported += batch.len() as u64;
        db.bulk_insert(table, batch).await?;
    }
    Ok(rows_imported)
}
```

### JSON y JSONL (JSON Lines)

```sql
-- Importar JSON array
COPY products FROM '/data/productos.json'
WITH (FORMAT json);

-- Importar JSONL (una fila JSON por línea — ideal para archivos grandes)
COPY logs FROM '/data/logs.jsonl'
WITH (FORMAT jsonl);

-- Importar con transformación (JSONPath mapping)
COPY users FROM '/data/export.json'
WITH (
  FORMAT json,
  MAPPING '{"nombre": "$.name", "email": "$.contact.email", "activo": "$.is_active"}'
);

-- Exportar como JSON array
COPY orders TO '/exports/pedidos.json'
WITH (FORMAT json, PRETTY true);

-- Exportar como JSONL (streaming, sin cargar todo en memoria)
COPY (SELECT * FROM logs WHERE level = 'ERROR')
TO '/exports/errores.jsonl'
WITH (FORMAT jsonl);
```

```rust
use serde_json::{Value, Deserializer};

async fn import_jsonl(path: &str, table: &str, db: &Database) -> Result<u64> {
    let file = tokio::fs::File::open(path).await?;
    let reader = tokio::io::BufReader::new(file);
    let schema = db.table_schema(table)?;
    let mut lines = reader.lines();
    let mut batch = Vec::with_capacity(1000);
    let mut count = 0u64;

    while let Some(line) = lines.next_line().await? {
        let json: Value = serde_json::from_str(&line)?;
        let row = schema.parse_json_object(&json)?;
        batch.push(row);

        if batch.len() == 1000 {
            db.bulk_insert(table, std::mem::take(&mut batch)).await?;
            count += 1000;
        }
    }
    if !batch.is_empty() {
        count += batch.len() as u64;
        db.bulk_insert(table, batch).await?;
    }
    Ok(count)
}
```

### Parquet — formato columnar (leer/escribir directamente)

```sql
-- Leer Parquet como tabla (sin importar — query directa como DuckDB)
SELECT * FROM READ_PARQUET('/data/ventas_2025.parquet')
WHERE total > 1000;

-- Exportar a Parquet (columnar, comprimido — ideal para analytics)
COPY (SELECT * FROM orders WHERE YEAR(created_at) = 2025)
TO '/exports/orders_2025.parquet'
WITH (FORMAT parquet, COMPRESSION snappy);
```

```rust
use parquet::file::reader::FileReader;
use parquet::record::RowAccessor;

fn read_parquet(path: &str) -> Result<impl Iterator<Item = Row>> {
    let file = File::open(path)?;
    let reader = SerializedFileReader::new(file)?;
    Ok(reader.get_row_iter(None)?.map(|row| parquet_row_to_db_row(row?)))
}
```

Crate: `parquet = "52"` (mismo ecosistema que Arrow).

### Backup y Restore completo

```sql
-- Backup completo (hot backup — sin lockear la BD)
BACKUP DATABASE mydb TO '/backups/mydb_20260321.backup'
WITH (FORMAT binary, COMPRESS true);

-- Backup incremental (solo cambios desde el último backup)
BACKUP DATABASE mydb TO '/backups/mydb_20260321_inc.backup'
WITH (FORMAT binary, INCREMENTAL true, SINCE '/backups/mydb_20260320.backup');

-- Restore
RESTORE DATABASE mydb FROM '/backups/mydb_20260321.backup';

-- Dump SQL (portable, texto legible)
DUMP DATABASE mydb TO '/backups/mydb.sql'
WITH (FORMAT sql, INCLUDE_SCHEMA true, INCLUDE_DATA true);

-- Cargar dump SQL
SOURCE '/backups/mydb.sql';
```

---

## Infraestructura — Connection Pooling

Sin pooler: 1000 clientes = 1000 conexiones TCP + 1000 tareas Tokio.
Con pooler integrado: 1000 clientes → 20 conexiones reales al motor.

```toml
[pool]
max_connections      = 20    # conexiones reales al motor
max_client_wait_ms   = 5000  # tiempo máximo en cola antes de error
idle_timeout_s       = 300   # cerrar conexiones inactivas >5min
max_lifetime_s       = 3600  # reciclar conexiones cada hora
```

```rust
struct ConnectionPool {
    connections: Arc<Semaphore>,        // límite de conexiones reales
    idle:        Mutex<Vec<Connection>>,
}

impl ConnectionPool {
    async fn acquire(&self) -> PooledConn {
        // Tomar del pool o crear nueva conexión (limitado por Semaphore)
        let _permit = self.connections.acquire().await.unwrap();
        let conn = self.idle.lock().pop()
            .unwrap_or_else(|| Connection::new());
        PooledConn { conn, pool: self.clone() }
    }
}

// Al soltar la conexión, vuelve al pool automáticamente
impl Drop for PooledConn {
    fn drop(&mut self) {
        self.pool.idle.lock().push(self.conn.take());
    }
}
```

---

## SQL Soportado

### DDL

```sql
CREATE TABLE nombre (
  col1  INT PRIMARY KEY,
  col2  VARCHAR(255) NOT NULL,
  col3  FLOAT DEFAULT 0.0,
  col4  JSON,
  col5  INT REFERENCES otra_tabla(id) ON DELETE CASCADE,
  UNIQUE (col2, col3)
);

CREATE INDEX nombre ON tabla(col1, col2);
CREATE INDEX nombre ON tabla(col1) WHERE col2 = 'activo';           -- partial index
CREATE INDEX nombre ON tabla(col1) INCLUDE (col2, col3);            -- covering index
CREATE INDEX nombre ON tabla(col1) WITH (fillfactor = 70);          -- fill factor
CREATE UNIQUE INDEX nombre ON users(email) WHERE deleted_at IS NULL; -- partial UNIQUE
REINDEX INDEX nombre;
REINDEX INDEX CONCURRENTLY nombre;
REINDEX TABLE nombre;
DROP TABLE nombre;
DROP INDEX nombre;
ALTER TABLE nombre ADD COLUMN col6 INT;                              -- non-blocking

-- Clonar estructura de tabla
CREATE TABLE orders_backup (LIKE orders INCLUDING ALL);
CREATE TABLE orders_2025 AS SELECT * FROM orders WHERE YEAR(created_at) = 2025;

-- Tabla particionada
CREATE TABLE logs (...) PARTITION BY RANGE (created_at);
CREATE TABLE logs_2025 PARTITION OF logs
  FOR VALUES FROM ('2025-01-01') TO ('2026-01-01');

-- Materialized view
CREATE MATERIALIZED VIEW resumen AS SELECT ...;
REFRESH MATERIALIZED VIEW resumen;

-- TTL
INSERT INTO sessions (...) VALUES (...) WITH TTL 3600;

-- Suscribirse a cambios
LISTEN canal;
NOTIFY canal, 'payload';

-- Git para datos
CALL dolt_commit('-m', 'mensaje');
CALL dolt_branch('feature');
CALL dolt_merge('feature');
```

### DML

```sql
-- Básico
INSERT INTO tabla (col1, col2) VALUES (1, 'texto');
SELECT * FROM tabla WHERE id = 1;
UPDATE tabla SET col2 = 'nuevo' WHERE id = 1;
DELETE FROM tabla WHERE id = 1;

-- Filtros
SELECT * FROM tabla WHERE age > 25 AND name LIKE 'A%';
SELECT * FROM tabla WHERE created_at BETWEEN '2024-01-01' AND '2024-12-31';
SELECT * FROM tabla WHERE status IN ('active', 'pending');

-- Joins
SELECT u.name, r.name AS role
FROM users u
JOIN roles r ON u.role_id = r.id
WHERE u.active = 1;

-- Agregaciones
SELECT COUNT(*), AVG(price), MAX(price)
FROM products
GROUP BY category_id
HAVING COUNT(*) > 5;

-- Ordenamiento y paginación
SELECT * FROM users ORDER BY name ASC LIMIT 20 OFFSET 40;
SELECT * FROM users ORDER BY created_at DESC NULLS LAST;   -- NULLs al final
SELECT * FROM users ORDER BY precio ASC NULLS FIRST;       -- NULLs al inicio

-- INSERT desde query (muy común)
INSERT INTO backup_orders SELECT * FROM orders WHERE created_at < '2025-01-01';
INSERT INTO user_stats (user_id, total)
  SELECT user_id, COUNT(*) FROM orders GROUP BY user_id
  ON CONFLICT (user_id) DO UPDATE SET total = EXCLUDED.total;

-- DISTINCT ON — primera fila por grupo (PostgreSQL style)
SELECT DISTINCT ON (user_id) *
FROM orders
ORDER BY user_id, created_at DESC;  -- el pedido más reciente por usuario

-- Funciones de comparación esenciales
SELECT COALESCE(apodo, nombre, 'Sin nombre') AS display_name FROM users;
SELECT NULLIF(division, 0) AS safe_div FROM metrics;   -- NULL si division=0
SELECT GREATEST(precio_web, precio_tienda) AS max_precio FROM products;
SELECT LEAST(fecha_envio, fecha_prometida) AS fecha_efectiva FROM orders;

-- Generadores
SELECT * FROM GENERATE_SERIES(1, 10) AS n;
SELECT * FROM GENERATE_SERIES('2026-01-01'::DATE, '2026-12-31', '1 month') AS mes;

-- Arrays expandidos a filas
SELECT id, UNNEST(tags) AS tag FROM posts;
SELECT ARRAY_TO_STRING(ARRAY['a','b','c'], ',');   -- 'a,b,c'
SELECT STRING_TO_ARRAY('a,b,c', ',');              -- ARRAY['a','b','c']
```

### Transacciones y Savepoints

```sql
BEGIN;
  INSERT INTO orders (user_id, total) VALUES (1, 150.0);

  SAVEPOINT antes_de_pago;
  UPDATE accounts SET balance = balance - 150 WHERE user_id = 1;

  -- algo sale mal:
  ROLLBACK TO antes_de_pago;  -- deshace el UPDATE, mantiene el INSERT

  -- intentar de nuevo con lógica diferente
  UPDATE accounts SET balance = balance - 150 WHERE user_id = 1 AND balance >= 150;
COMMIT;
```

### CTEs y CTEs Recursivos

```sql
-- CTE simple (WITH)
WITH pedidos_grandes AS (
  SELECT user_id, SUM(total) AS total_gastado
  FROM orders WHERE total > 1000
  GROUP BY user_id
)
SELECT u.nombre, p.total_gastado
FROM users u JOIN pedidos_grandes p ON u.id = p.user_id
ORDER BY p.total_gastado DESC;

-- CTE recursivo — árbol de categorías / jerarquía de empleados
WITH RECURSIVE categoria_tree AS (
  -- caso base: raíz
  SELECT id, nombre, parent_id, 0 AS nivel, nombre AS path
  FROM categorias WHERE parent_id IS NULL
  UNION ALL
  -- caso recursivo: hijos
  SELECT c.id, c.nombre, c.parent_id, t.nivel + 1,
         t.path || ' > ' || c.nombre
  FROM categorias c
  JOIN categoria_tree t ON c.parent_id = t.id
)
SELECT * FROM categoria_tree ORDER BY path;
```

### RETURNING

```sql
-- Obtener el ID generado sin segundo query
INSERT INTO orders (user_id, total) VALUES (1, 150.0)
RETURNING id, created_at;

-- UPDATE con resultado
UPDATE users SET last_login = NOW(), login_count = login_count + 1
WHERE email = 'ana@gmail.com'
RETURNING id, login_count, last_login;

-- DELETE con confirmación
DELETE FROM sessions WHERE expires_at < NOW()
RETURNING id, user_id;
```

### MERGE / UPSERT

```sql
-- PostgreSQL style (INSERT ... ON CONFLICT)
INSERT INTO products (id, nombre, stock)
VALUES (1, 'Biblia RV60', 50)
ON CONFLICT (id) DO UPDATE
  SET stock = products.stock + EXCLUDED.stock,
      updated_at = NOW();

-- ON CONFLICT DO NOTHING
INSERT INTO tags (nombre) VALUES ('rust')
ON CONFLICT (nombre) DO NOTHING;

-- MERGE estándar SQL
MERGE INTO inventory AS target
USING nuevos_stocks AS source ON target.product_id = source.product_id
WHEN MATCHED AND source.stock > 0
  THEN UPDATE SET stock = source.stock, updated_at = NOW()
WHEN MATCHED AND source.stock = 0
  THEN DELETE
WHEN NOT MATCHED
  THEN INSERT (product_id, stock) VALUES (source.product_id, source.stock);
```

### CHECK Constraints y DOMAIN types

```sql
-- CHECK en columna
CREATE TABLE users (
  age    INT  CHECK (age > 0 AND age < 150),
  salary DECIMAL CHECK (salary >= 0)
);

-- CHECK a nivel de tabla (múltiples columnas)
CREATE TABLE reservations (
  start_date DATE,
  end_date   DATE,
  CHECK (end_date > start_date)
);

-- DOMAIN — tipo reutilizable con validación
CREATE DOMAIN email_type AS TEXT
  CHECK (VALUE LIKE '%@%' AND LENGTH(VALUE) < 255);

CREATE DOMAIN precio_type AS DECIMAL(10,2)
  CHECK (VALUE >= 0);

CREATE TABLE products (
  nombre TEXT,
  precio precio_type,   -- validación automática
  contacto email_type
);
```

### Tablas Temporales y Sin Log

```sql
-- TEMP: solo visible en la sesión actual, se borra al cerrar
CREATE TEMP TABLE resultados_tmp (
  id    INT,
  score FLOAT
);

-- UNLOGGED: sin WAL → más rápido, pero se pierde en crash
-- Ideal para caches, sesiones, datos regenerables
CREATE UNLOGGED TABLE session_cache (
  session_id TEXT PRIMARY KEY,
  user_id    INT,
  data       JSON,
  expires_at TIMESTAMP
);
```

### Expression Indexes

```sql
-- Índice sobre expresión calculada
CREATE INDEX idx_email_lower    ON users(LOWER(email));
CREATE INDEX idx_nombre_tsvector ON docs(TO_TSVECTOR(contenido));
CREATE INDEX idx_year_created   ON orders(YEAR(created_at));
CREATE INDEX idx_full_name      ON users(nombre || ' ' || apellido);

-- El planner lo usa automáticamente:
SELECT * FROM users WHERE LOWER(email) = 'ana@gmail.com';
-- → usa idx_email_lower, no full scan
```

### LATERAL Joins

```sql
-- Por cada usuario, obtener sus últimos 3 pedidos
SELECT u.nombre, ult.total, ult.created_at
FROM users u
JOIN LATERAL (
  SELECT total, created_at FROM orders
  WHERE user_id = u.id
  ORDER BY created_at DESC LIMIT 3
) AS ult ON true;

-- Top producto por categoría
SELECT c.nombre AS categoria, top.nombre AS producto, top.ventas
FROM categorias c
JOIN LATERAL (
  SELECT nombre, COUNT(*) AS ventas FROM products p
  JOIN order_items oi ON p.id = oi.product_id
  WHERE p.categoria_id = c.id
  GROUP BY nombre ORDER BY ventas DESC LIMIT 1
) AS top ON true;
```

### Cursores

```sql
-- Para resultados grandes sin cargar todo en memoria
DECLARE logs_cursor CURSOR FOR
  SELECT id, message, created_at FROM logs
  WHERE level = 'ERROR'
  ORDER BY created_at DESC;

FETCH 100 FROM logs_cursor;   -- leer 100 filas
FETCH NEXT FROM logs_cursor;  -- siguiente fila
FETCH ALL  FROM logs_cursor;  -- resto completo

CLOSE logs_cursor;
```

```rust
struct Cursor {
    plan:     PhysicalPlan,
    position: u64,
    batch:    VecDeque<Row>,
}

impl Cursor {
    fn fetch(&mut self, count: usize) -> Result<Vec<Row>> {
        if self.batch.len() < count {
            self.refill_batch(count * 2)?;
        }
        Ok(self.batch.drain(..count.min(self.batch.len())).collect())
    }
}
```

### Query Hints

```sql
-- Forzar uso de índice específico
SELECT /*+ INDEX(orders idx_user_id) */ *
FROM orders WHERE user_id = 5;

-- Forzar full scan (cuando el planner elige mal)
SELECT /*+ FULL_SCAN(products) */ *
FROM products WHERE stock > 0;

-- Forzar tipo de join
SELECT /*+ HASH_JOIN(u, r) */ u.nombre, r.nombre
FROM users u JOIN roles r ON u.role_id = r.id;

-- Parallelism hint
SELECT /*+ PARALLEL(4) */ SUM(total) FROM orders;
```

---

## Crates de Rust recomendados

```toml
[dependencies]
# Async runtime para conexiones concurrentes
tokio = { version = "1", features = ["full"] }

# Parser SQL zero-copy (muy rápido, usado por proyectos serios)
nom = "7"

# mmap portable Linux/Mac/Windows
memmap2 = "0.9"

# Cast seguro entre bytes y structs (zero-copy)
bytemuck = "1"

# Serialización binaria rápida para páginas
bincode = "2"

# CRC32 para checksums de páginas
crc32fast = "1"

# Para implementar MySQL wire protocol
tokio-util = "0.7"
bytes = "1"

# SIMD portable (AVX2, SSE4, NEON — sin unsafe manual)
wide = "0.7"

# Paralelismo de datos (morsel-driven)
rayon = "1"

# Compresión LZ4 para TOAST (valores grandes)
lz4_flex = "0.11"

# Bloom filters para índices
bloomfilter = "1"

# JSON nativo
serde_json = "1"
jsonpath-lib = "0.3"

# Full-Text Search (opción A: librería completa tipo Lucene)
tantivy = "0.22"
# Full-Text Search (opción B: implementación propia más didáctica)
# usar bincode para serializar el índice invertido propio

# Apache Arrow — formato columnar para resultados
arrow = "52"

# LRU cache para modo in-memory
lru = "0.12"

# Git versioning para datos (DoltDB-inspired)
sha1 = "0.10"

# Lua scripting para EVAL atómico
mlua = { version = "0.9", features = ["lua54", "vendored"] }

# WASM plugins — sandboxed functions en cualquier lenguaje
wasmtime = "20"

# TLS — conexiones cifradas
tokio-rustls = "0.26"
rustls = "0.23"

# Argon2id para hashing de passwords
argon2 = "0.5"

# CSV import/export
csv = "1"

# Parquet import/export (mismo ecosistema que Arrow)
parquet = "52"

# Concurrent HashMap para pg_stat_statements
dashmap = "5"

# Vector similarity search (HNSW)
hnsw_rs = "0.3"

# Scheduled jobs (cron)
tokio-cron-scheduler = "0.11"

# Foreign Data Wrappers — HTTP
reqwest = { version = "0.12", features = ["json", "blocking"] }

# Migración desde MySQL
mysql_async = "0.34"

# Migración desde PostgreSQL / wire protocol
tokio-postgres = "0.7"

# DECIMAL exacto para dinero y contabilidad
rust_decimal = "1"

# Strings cortos en stack sin heap allocation (≤23 chars)
compact_str = "0.8"

# UUID v4 y v7
uuid = { version = "1", features = ["v4", "v7"] }

# Fecha y hora correcta con INTERVAL y TIMESTAMPTZ
time = { version = "0.3", features = ["macros"] }

# Tipos de red INET/CIDR (ya en std::net para IpAddr)
# MACADDR: [u8; 6] nativo sin crate adicional

# Bit vectors para BIT/VARBIT y NULL bitmap
bitvec = "1"

# Zero-copy deserialization para nodos B+ Tree
rkyv = "0.8"

# DECIMAL fast path exacto
rust_decimal = "1"
rust_decimal_macros = "1"

# Collations Unicode internacionales
icu_collator = "1"

# f16 para vectores de media precisión
half = "2"

# Cotejamiento completo
icu_collator = "1"
icu_casemap = "1"                    # UPPER/LOWER locale-aware
unicode-normalization = "0.1"        # NFC/NFD/NFKC
encoding_rs = "0.8"                  # conversión latin1/utf16 → utf8

# Timezone database embebida (portable, sin depender del OS)
tzdata = "0.1"
chrono-tz = "0.9"

# Funciones matemáticas — RANDOM, SETSEED
rand = "0.8"

# Expresiones regulares — REGEXP_MATCH, REGEXP_REPLACE
regex = "1"

# Cifrado en reposo
aes-gcm = "0.10"
pbkdf2  = "0.12"

# Geospatial — R-Tree
rstar = "0.12"

# Tracing / observabilidad estructurada
tracing            = "0.1"
tracing-subscriber = "0.3"

# AI backends
ollama-rs    = "0.2"         # Ollama local (llama, nomic-embed-text, etc.)
async-openai = "0.23"        # OpenAI API (embeddings, GPT)

# ONNX model serving
ort = "2"                    # ONNX Runtime — corre modelos de PyTorch/sklearn

# Privacidad diferencial
opendp = "0.10"

# DSN / connection string parsing
url = "2"

# Sharding y distribución
# (implementación propia sobre tokio + tonic para gRPC entre nodos)
tonic = "0.12"               # gRPC para comunicación inter-nodo
prost = "0.13"               # protobuf para serialización de mensajes

# Semver para extensions
semver = "1"

# JIT compilation (requiere LLVM instalado — fase final)
inkwell = { version = "0.4", optional = true }

[features]
jit      = ["inkwell"]   # cargo build --features jit
fts      = ["tantivy"]   # cargo build --features fts
arrow    = ["dep:arrow"] # cargo build --features arrow
dolt     = []            # git versioning para datos
```

---

## Optimizaciones inspiradas en DuckDB

### 1. Morsel-Driven Parallelism

En vez de que un solo thread haga el scan completo, el dataset se parte en
**morsels** (trozos de ~100K filas) y Rayon los distribuye entre todos los cores.

```rust
use rayon::prelude::*;

fn parallel_scan(table: &Table, predicate: &Predicate) -> Vec<Row> {
    table
        .morsels(100_000)          // divide en chunks de 100K filas
        .par_iter()                // Rayon: un thread por morsel
        .flat_map(|morsel| {
            vectorized_filter(morsel, predicate)  // SIMD dentro de cada morsel
        })
        .collect()
}
```

**Ganancia:** Escala linealmente con cores. En 8 cores → ~7x vs single-thread.
Tokio maneja I/O async; Rayon maneja CPU parallelism. Usan thread pools separados.

---

### 2. Operator Fusion (Pipeline Breakers)

En vez de materializar resultados intermedios entre operadores, se fusionan
en un solo loop. Menos memoria, mejor uso del cache del CPU.

```
Sin fusion (tradicional):
  SCAN  → [buffer 1M rows] → FILTER → [buffer 500K rows] → PROJECT → resultado

Con fusion:
  for morsel in table:
      row = scan(morsel)
      if filter(row):          ← mismo loop
          emit project(row)    ← mismo loop, sin buffers intermedios
```

```rust
// Pipeline fusionado: scan + filter + project en un solo iterador lazy
fn fused_pipeline<'a>(
    table: &'a Table,
    predicate: &'a Predicate,
    columns: &'a [usize],
) -> impl ParallelIterator<Item = Row> + 'a {
    table
        .morsels(100_000)
        .par_iter()
        .flat_map(|morsel| {
            morsel
                .rows()
                .filter(|row| predicate.eval(row))   // filter inline
                .map(|row| row.project(columns))      // project inline
        })
}
```

**Ganancia:** Elimina buffers intermedios. En queries con WHERE + SELECT específico,
reduce uso de memoria 5-10x y mejora cache hit rate.

---

### 3. Late Materialization

Evaluar predicados sobre columnas baratas (int, bool) **antes** de leer columnas
caras (VARCHAR, TEXT, BLOB). Solo materializar la fila completa para las que pasen.

```
Query: SELECT name, bio FROM users WHERE age > 25 AND active = 1

Sin late materialization:
  1. Leer fila completa (id, age, active, name, bio, ...)  ← lee bio siempre
  2. Aplicar WHERE

Con late materialization:
  1. Leer solo columna `age`    (4 bytes × N filas)
  2. Aplicar age > 25           → máscara de bits
  3. Leer solo columna `active` (1 byte × filas que pasaron)
  4. Aplicar active = 1         → máscara reducida
  5. Leer name, bio             → SOLO para filas que pasaron ambos filtros
```

```rust
struct QueryPlan {
    predicates_cheap: Vec<Predicate>,  // sobre columnas numéricas/bool
    predicates_expensive: Vec<Predicate>, // sobre VARCHAR/TEXT
    project_columns: Vec<usize>,
}

fn late_materialization(table: &Table, plan: &QueryPlan) -> Vec<Row> {
    // Paso 1: filtrar con predicados baratos (solo esas columnas)
    let candidate_ids: BitVec = table
        .column_scan(&plan.cheap_columns())
        .simd_filter(&plan.predicates_cheap);

    // Paso 2: filtrar con predicados caros (solo filas candidatas)
    let final_ids: BitVec = table
        .column_scan_masked(&plan.expensive_columns(), &candidate_ids)
        .filter(&plan.predicates_expensive);

    // Paso 3: materializar solo columnas proyectadas, solo filas finales
    table.materialize(&plan.project_columns, &final_ids)
}
```

**Ganancia:** Si solo el 5% de filas pasan el WHERE, lees las columnas caras
solo para ese 5%. En tablas con VARCHAR grandes → 10-20x menos I/O.

---

## Plan de desarrollo (fases)

```
Fase 1 — Storage básico             (semana 1-2)
  ✓ Leer/escribir páginas en disco
  ✓ mmap del archivo .db
  ✓ Formato de página con checksum
  ✓ Free list de páginas

Fase 2 — B+ Tree                    (semana 3-4)
  ✓ Nodos internos y hojas
  ✓ Insert con split de nodos
  ✓ Lookup por key exacto
  ✓ Range scan por linked list de hojas
  ✓ Delete con merge/redistribución

Fase 3 — WAL y transacciones        (semana 5)
  ✓ Append-only WAL
  ✓ BEGIN / COMMIT / ROLLBACK
  ✓ Crash recovery (replay del WAL)

Fase 4 — SQL Parser + Executor      (semana 6-7)
  ✓ Parser para DDL y DML básico
  ✓ Executor conectado al storage
  ✓ CLI interactiva (como sqlite3 shell)

Fase 5 — MySQL wire protocol        (semana 8)
  ✓ TCP server en Tokio
  ✓ MySQL handshake + autenticación básica
  ✓ PHP y Python conectan sin driver custom

Fase 6 — Índices secundarios + FK   (semana 9)
  ✓ Múltiples B+ Trees por tabla
  ✓ Validación de FK en INSERT/UPDATE/DELETE
  ✓ ON DELETE CASCADE / RESTRICT

Fase 7 — Concurrencia + MVCC        (semana 10)
  ✓ Copy-on-Write en B+ Tree
  ✓ Snapshot isolation para reads
  ✓ Múltiples writers con serialización

Fase 8 — Optimizaciones SIMD        (semana 11-12)
  ✓ Vectorized execution en table scans
  ✓ SIMD para predicados simples (AVX2 con crate `wide`)
  ✓ Query planner básico (usar índice vs full scan)
  ✓ Benchmarks vs MySQL

Fase 9 — DuckDB-inspired            (semana 13-14)
  ✓ Morsel-driven parallelism (Rayon, un morsel por core)
  ✓ Operator fusion (scan+filter+project en un pipeline lazy)
  ✓ Late materialization (predicados baratos primero, materializar al final)
  ✓ Benchmarks actualizados con paralelismo

Fase 10 — Modo embebido + FFI       (semana 15-16)
  ✓ Refactor del motor como crate reutilizable (lib.rs)
  ✓ C FFI: dbyo_open / dbyo_execute / dbyo_close
  ✓ Compilar como cdylib (.so / .dll / .dylib)
  ✓ Binding Python (ctypes) para pruebas
  ✓ Binding Node.js via Neon (opcional, para Electron)
  ✓ Tests: misma BD usada desde servidor y desde librería

Fase 11 — Robustez (RocksDB + SQLite inspired)  (semana 17-18)
  ✓ Bloom filters en cada índice B+ Tree
  ✓ Sparse index para columnas de timestamp/secuencia
  ✓ Prefix compression en nodos internos del B+ Tree
  ✓ TOAST: valores >2KB a páginas de overflow con LZ4
  ✓ In-memory mode (":memory:")
  ✓ JSON como tipo nativo con extracción por path
  ✓ Partial indexes con predicado en CREATE INDEX
  ✓ Full-Text Search: tokenizer + índice invertido + BM25 ranking
  ✓ CREATE VIRTUAL TABLE ... USING fts(...)
  ✓ MATCH operator con soporte de frases, booleanos y prefijos

Fase 12 — Testing + JIT             (semana 19-20)
  ✓ Deterministic simulation testing (FaultInjector con semilla)
  ✓ EXPLAIN ANALYZE con tiempos reales por nodo
  ✓ JIT compilation básica con LLVM (predicados simples)
  ✓ Benchmarks finales vs MySQL y SQLite

Fase 13 — PostgreSQL avanzado       (semana 21-22)
  ✓ Materialized views (CREATE MATERIALIZED VIEW + REFRESH)
  ✓ Window functions (RANK, ROW_NUMBER, LAG, LEAD, SUM OVER)
  ✓ Generated/computed columns (STORED y VIRTUAL)
  ✓ LISTEN / NOTIFY pub-sub nativo
  ✓ Covering indexes (INCLUDE columns en B+ Tree hojas)
  ✓ Non-blocking ALTER TABLE (shadow table + WAL delta)

Fase 14 — TimescaleDB + Redis       (semana 23-24)
  ✓ Table partitioning (RANGE, HASH, LIST)
  ✓ Partition pruning en el query planner
  ✓ Compresión automática de particiones históricas (LZ4 columnar)
  ✓ Continuous aggregates con refresh incremental
  ✓ TTL por fila con background reaper en Tokio
  ✓ LRU eviction para modo in-memory

Fase 15 — MongoDB + DoltDB + Arrow  (semana 25-26)
  ✓ Change streams CDC basado en WAL
  ✓ Git para datos: commits, branches, checkout, merge, diff
  ✓ Apache Arrow como formato de salida para queries analíticas
  ✓ Benchmarks finales completos vs MySQL, SQLite, DuckDB

Fase 16 — Lógica del servidor       (semana 27-29)
  ✓ SQL UDFs escalares y de tabla (CREATE FUNCTION ... AS $$ ... $$)
  ✓ Triggers BEFORE/AFTER con WHEN condicional y SIGNAL para errores
  ✓ Lua runtime (mlua): EVAL con query() y execute() atómicos
  ✓ WASM runtime (wasmtime): CREATE FUNCTION LANGUAGE wasm FROM FILE
  ✓ Sandbox WASM: límites de memoria, timeout, sin acceso externo
  ✓ Tests: plugin de riesgo crediticio en Rust compilado a WASM

Fase 17 — Seguridad                 (semana 30-31)
  ✓ CREATE USER / CREATE ROLE / GRANT / REVOKE
  ✓ Permisos por tabla y por columna
  ✓ Row-Level Security con políticas por tabla
  ✓ Autenticación Argon2id + Scram-SHA-256 en wire protocol
  ✓ TLS 1.3 para todas las conexiones (tokio-rustls)
  ✓ Statement timeout por usuario/sesión/global

Fase 18 — Alta disponibilidad       (semana 32-33)
  ✓ Streaming replication (primary → replicas vía WAL)
  ✓ Réplicas sincrónicas y asincrónicas configurables
  ✓ Point-in-Time Recovery (PITR) usando WAL acumulado
  ✓ Hot backup sin lockear la BD
  ✓ Dump SQL portable (SOURCE / DUMP)

Fase 19 — Mantenimiento + observabilidad  (semana 34-35)
  ✓ Auto-vacuum en Tokio background task
  ✓ VACUUM / VACUUM ANALYZE / VACUUM FULL
  ✓ Deadlock detection con grafo de espera (DFS, cada 100ms)
  ✓ pg_stat_statements: calls, tiempo total, cache hits
  ✓ Slow query log en JSON con plan de ejecución
  ✓ Connection pooling integrado (Semaphore + idle pool)

Fase 20 — Tipos + importación/exportación  (semana 36-37)
  ✓ Views regulares (CREATE VIEW) y actualizables
  ✓ Sequences (CREATE SEQUENCE, NEXTVAL, CURRVAL)
  ✓ ENUMs (CREATE TYPE ... AS ENUM)
  ✓ Arrays (TEXT[], FLOAT[], ANY(), @>)
  ✓ COPY FROM/TO: CSV, JSON, JSONL, Parquet
  ✓ READ_PARQUET() como función de tabla directa
  ✓ Backup incremental y restore completo

Fase 21 — SQL avanzado              (semana 38-39)
  ✓ Savepoints (SAVEPOINT / ROLLBACK TO / RELEASE)
  ✓ CTEs (WITH) y CTEs recursivos (WITH RECURSIVE)
  ✓ RETURNING en INSERT / UPDATE / DELETE
  ✓ MERGE / UPSERT (INSERT ON CONFLICT + MERGE estándar)
  ✓ CHECK constraints y DOMAIN types
  ✓ Tablas temporales (TEMP) y sin log (UNLOGGED)
  ✓ Expression indexes (índices sobre LOWER(), YEAR(), etc.)
  ✓ LATERAL joins
  ✓ Cursores (DECLARE / FETCH / CLOSE)
  ✓ Query hints (/*+ INDEX() HASH_JOIN() PARALLEL() */)

Fase 22 — Features de producto      (semana 40-42)
  ✓ Vector similarity search: tipo VECTOR(n), índice HNSW, operador <=>
  ✓ Búsqueda fuzzy: SIMILARITY(), trigramas GIN index
  ✓ Scheduled jobs: cron_schedule() con expresiones cron
  ✓ Foreign Data Wrappers: CREATE FOREIGN TABLE ... SERVER
  ✓ Multi-database: CREATE DATABASE, USE, cross-database queries
  ✓ Schema namespacing: CREATE SCHEMA, schema.tabla
  ✓ Schema migrations CLI: dbyo migrate up/down/status
  ✓ GraphQL API nativa — puerto :3308, schema autodescubierto, queries/mutations/subscriptions
  ✓ GraphQL subscriptions vía WAL stream — WebSocket, eventos en tiempo real sin polling
  ✓ GraphQL DataLoader integrado — batch loading automático, cero N+1
  ✓ GraphQL introspection — compatible con Apollo Studio, Postman, codegen
  ✓ OData v4 nativo — puerto :3309, PowerBI/Excel/Tableau/SAP sin drivers ni ODBC
  ✓ OData $metadata — EDMX autodescubierto desde catálogo (PowerBI lo usa al conectar)
  ✓ OData $filter/$select/$orderby/$top/$skip/$count/$expand/$batch

---

## OData v4 API Nativa

### Por qué OData

OData (Open Data Protocol) es el estándar REST usado por PowerBI, Excel Power Query,
Tableau, SAP, Microsoft Dynamics y prácticamente todo el ecosistema enterprise.
Con un endpoint OData, NexusDB se conecta a PowerBI sin drivers, sin ODBC, sin gateway —
el analista escribe la URL y tiene sus datos en segundos.

### Arquitectura

```
PowerBI / Excel / Tableau / SAP
        │
        │ HTTP  :3309
        │ GET /odata/$metadata        ← descubrir schema
        │ GET /odata/users?$filter=.. ← query con filtros
        │ GET /odata/orders?$expand=customer ← JOIN por FK
        ▼
┌─────────────────────────────┐
│     OData v4 Server         │
│  (axum + Tokio)             │
│                             │
│  $metadata ← Catálogo       │  ← documento EDMX autodescubierto
│  $filter   → WHERE clause   │  ← parser OData → AST SQL
│  $expand   → JOIN por FK    │  ← el catálogo conoce las FKs
│  $orderby  → ORDER BY       │
│  $top/$skip → LIMIT/OFFSET  │
│  $count    → COUNT(*)       │
│  $select   → column pruning │
└─────────────────────────────┘
        │
        ▼
   Motor NexusDB (compartido con MySQL, PostgreSQL, GraphQL)
```

### Endpoint $metadata — autodescubierto

PowerBI llama a `GET /odata/$metadata` al conectar. NexusDB genera el documento
EDMX desde el catálogo de tablas sin configuración manual:

```xml
<!-- GET /odata/$metadata -->
<edmx:Edmx Version="4.0">
  <edmx:DataServices>
    <Schema Namespace="NexusDB">
      <EntityType Name="User">
        <Key><PropertyRef Name="Id"/></Key>
        <Property Name="Id" Type="Edm.Guid" Nullable="false"/>
        <Property Name="Name" Type="Edm.String"/>
        <Property Name="Email" Type="Edm.String"/>
        <Property Name="CreatedAt" Type="Edm.DateTimeOffset"/>
        <NavigationProperty Name="Orders" Type="Collection(NexusDB.Order)"/>
      </EntityType>
      <EntitySet Name="Users" EntityType="NexusDB.User"/>
    </Schema>
  </edmx:DataServices>
</edmx:Edmx>
```

### Queries OData → SQL

```
GET /odata/users?$filter=age gt 25 and country eq 'CO'
→ SELECT * FROM users WHERE age > 25 AND country = 'CO'

GET /odata/orders?$select=id,total&$orderby=total desc&$top=10&$skip=20
→ SELECT id, total FROM orders ORDER BY total DESC LIMIT 10 OFFSET 20

GET /odata/orders?$expand=customer&$filter=total gt 100
→ SELECT orders.*, customers.* FROM orders
  JOIN customers ON orders.customer_id = customers.id
  WHERE orders.total > 100

GET /odata/users/$count
→ SELECT COUNT(*) FROM users
```

### Conexión desde PowerBI

```
1. PowerBI → Obtener datos → OData Feed
2. URL: http://servidor:3309/odata
3. PowerBI llama a /odata/$metadata → descubre tablas automáticamente
4. El analista elige qué tablas importar
5. PowerBI genera queries OData → NexusDB las traduce a SQL → retorna JSON
6. Sin drivers, sin ODBC, sin gateway — funciona en cualquier OS
```

### Tipos OData ↔ NexusDB

| Tipo NexusDB | Tipo OData (Edm) |
|---|---|
| INT, BIGINT | Edm.Int32, Edm.Int64 |
| REAL, DOUBLE | Edm.Single, Edm.Double |
| DECIMAL | Edm.Decimal |
| TEXT, VARCHAR | Edm.String |
| BOOL | Edm.Boolean |
| DATE | Edm.Date |
| TIMESTAMPTZ | Edm.DateTimeOffset |
| UUID | Edm.Guid |
| BYTEA | Edm.Binary |

### Puerto y configuración

```toml
# nexusdb.toml
[odata]
enabled  = true
port     = 3309
path     = "/odata"
auth     = "bearer"   # bearer, basic, none
max_page = 1000       # máximo de filas por respuesta ($top implícito)
```

### Diferenciador

| Herramienta | Forma de conectar a una BD | Con NexusDB OData |
|---|---|---|
| PowerBI | ODBC driver + gateway + configuración | URL directa, cero instalación |
| Excel | Complemento + driver | Datos → OData Feed → URL |
| Tableau | Driver específico por BD | Conector Web Data Connector |
| SAP | Adaptador custom | Endpoint estándar OData v4 |

---

## GraphQL API Nativa

### Por qué tiene sentido en NexusDB

NexusDB ya expone el mismo motor por dos vías (MySQL wire protocol + C FFI embebido).
GraphQL es un tercer protocolo de acceso que se construye encima de todo lo ya hecho,
sin duplicar lógica. El WAL ya es un event bus — las subscriptions son una consecuencia
natural de leerlo como stream.

### Arquitectura

```
Cliente (web/mobile/backend)
        │
        │ WebSocket / HTTP  :3308
        ▼
┌─────────────────────────────┐
│     GraphQL Server          │
│  (async-graphql + Tokio)    │
│                             │
│  Schema ← Catálogo tablas   │  ← autodescubierto en runtime
│  Queries → B+ Tree lookups  │  ← reutiliza el executor SQL
│  Mutations → WAL + B+ Tree  │  ← misma ruta que INSERT/UPDATE/DELETE
│  Subscriptions → WAL stream │  ← lee el WAL como canal de eventos
└─────────────────────────────┘
        │
        ▼
   Motor NexusDB (compartido)
```

### Schema autodescubierto

El schema GraphQL se genera automáticamente desde el catálogo de tablas:

```graphql
# Tabla SQL:
# CREATE TABLE users (id UUID, name TEXT, email TEXT, created_at TIMESTAMPTZ);

# Schema GraphQL generado automáticamente:
type User {
  id: ID!
  name: String!
  email: String!
  createdAt: String!
}

type Query {
  user(id: ID!): User
  users(limit: Int, offset: Int, orderBy: String): [User!]!
  usersWhere(filter: UserFilter): [User!]!
}

type Mutation {
  insertUser(input: UserInput!): User!
  updateUser(id: ID!, input: UserInput!): User
  deleteUser(id: ID!): Boolean!
}

type Subscription {
  onUserChange(filter: UserFilter): UserChangeEvent!
}
```

### Subscriptions vía WAL

```rust
// El WAL reader expone un canal de eventos:
async fn subscribe_table(table_id: u32) -> impl Stream<Item = WalEntry> {
    // Tail del WAL desde la posición actual
    // Cada COMMIT con entries de esta tabla emite un evento
    WalReader::tail(table_id)
}

// GraphQL subscription:
// subscription { onUserChange { id name email } }
// → lee el WAL stream → filtra por table_id → emite al WebSocket
```

### DataLoader — eliminar N+1

```rust
// Sin DataLoader (N+1):
// query { users { id orders { id total } } }
// → 1 query para users + N queries para orders de cada user = N+1

// Con DataLoader integrado:
// → 1 query para users + 1 query batch para todos sus orders
// El motor conoce el FK schema → batch automático sin configuración del cliente
```

### Crate elegido

`async-graphql` — el crate más completo del ecosistema Rust:
- Subscriptions con WebSocket nativo
- DataLoader integrado
- Introspection completo
- Compatible con Apollo Studio

### Puerto y configuración

```toml
# nexusdb.toml
[graphql]
enabled = true
port    = 3308
path    = "/graphql"
ws_path = "/graphql/ws"
introspection = true   # deshabilitar en producción si se desea
max_complexity = 100   # límite de complejidad de queries (protección DoS)
max_depth = 10         # profundidad máxima de queries anidadas
```

### Diferenciador vs competencia

| Característica | Hasura | PostGraphile | NexusDB GraphQL |
|---|---|---|---|
| Arquitectura | Encima de Postgres | Encima de Postgres | Nativo dentro del motor |
| Subscriptions | Polling o logical decoding | Polling | WAL stream directo |
| N+1 | DataLoader manual | DataLoader manual | Automático (motor conoce FKs) |
| Setup | Servicio separado | Plugin Postgres | Un binario, un puerto más |
| Latencia | +1 hop de red | +1 hop de red | In-process, sin hop |

Fase 23 — Retrocompatibilidad       (semana 43-45)
  ✓ Lector nativo de archivos SQLite (.db/.sqlite) sin libsqlite3
  ✓ ATTACH sqlite_file AS src USING sqlite
  ✓ Migración desde MySQL: conectar live + leer INFORMATION_SCHEMA
  ✓ Migración desde PostgreSQL: conectar live vía tokio-postgres
  ✓ CLI: dbyo migrate from-mysql / from-postgres / from-sqlite
  ✓ PostgreSQL wire protocol en puerto 5432 (psql, psycopg2, pgx)
  ✓ Puerto 3306 MySQL + Puerto 5432 PostgreSQL simultáneos

Fase 24 — Sistema de tipos completo (semana 46-48)
  ✓ Enteros: TINYINT, SMALLINT, BIGINT, HUGEINT + variantes U (sin signo)
  ✓ REAL/FLOAT4 (f32) separado de DOUBLE (f64)
  ✓ DECIMAL exacto con rust_decimal (para dinero y contabilidad)
  ✓ CITEXT — texto case-insensitive
  ✓ BYTEA/BLOB — binario con TOAST automático
  ✓ BIT(n) y VARBIT(n) — cadenas de bits
  ✓ TIMESTAMPTZ — siempre UTC interno, convierte al mostrar
  ✓ INTERVAL con meses/días/µs separados (aritmética de calendario)
  ✓ UUID v4 y v7 (v7 ordenable → mejor rendimiento como PK)
  ✓ INET, CIDR, MACADDR — tipos de red
  ✓ RANGE(T) — int4range, daterange, tsrange con operadores @> &&
  ✓ COMPOSITE types — CREATE TYPE direccion AS (calle TEXT, ciudad TEXT)
  ✓ Value enum compacto de 24 bytes con CompactStr + Arc
  ✓ NULL bitmap por fila (en vez de Option<T> por valor)
  ✓ Column encoding automático: Dictionary, RLE, Delta, BitPacking

Fase 25 — Optimizaciones de tipos   (semana 49-51)
  ✓ VarInt encoding: enteros con tamaño variable (1-9 bytes según valor)
  ✓ Zigzag encoding para enteros con signo negativos
  ✓ DECIMAL fast path: i64+scale para ≤18 dígitos (20x más rápido)
  ✓ JSONB: formato binario pre-parseado con tabla de offsets
  ✓ VECTOR cuantización: f16 (2x ahorro) e int8 (4x ahorro)
  ✓ PAX layout: columnar dentro de cada página 8KB
  ✓ Estadísticas por columna: histogram, correlación, most_common
  ✓ ANALYZE automático y manual por columna
  ✓ Collations con ICU4X: es_419, en_US, zh_Hans, ar...
  ✓ Zero-copy deserialization con rkyv para nodos B+ Tree
  ✓ Compresión específica por tipo: Delta, BitPack, LZ4, ZSTD
  ✓ Sugerencias de tipo mínimo en EXPLAIN ANALYZE

Fase 26 — Cotejamiento completo     (semana 52-54)
  ✓ Niveles Unicode CLDR: Primary/Secondary/Tertiary/Quaternary
  ✓ Sufijos _ci, _cs, _ai, _as, _bin por columna/tabla/BD
  ✓ Configuración en cascada: servidor → BD → tabla → columna → query
  ✓ Encodings: utf8mb4, latin1, ascii, utf16 con conversión automática
  ✓ Unicode Normalization: NFC al guardar, NFKC para búsqueda
  ✓ Strip accents para búsqueda accent-insensitive
  ✓ Sort keys en B+ Tree (memcmp correcto con collation)
  ✓ UPPER/LOWER con ICU4X locale-aware (UPPER('josé') → 'JOSÉ')
  ✓ LENGTH en codepoints Unicode (no en bytes)
  ✓ LIKE respeta collation (jos% encuentra 'José González')
  ✓ Collation en FTS: tokenizer normaliza antes de indexar
  ✓ Detección automática de charset del cliente en handshake
  ✓ ~20 collations: es_419, es_ES, en_US, pt_BR, fr_FR, de_DE, zh_Hans, ar...

Fase 27 — Query Optimizer real      (semana 55-57)
  ✓ Join ordering con programación dinámica (2^N subconjuntos)
  ✓ Predicate pushdown (mover filtros cerca de los datos)
  ✓ Subquery unnesting (convertir subqueries a JOINs)
  ✓ Join elimination (FK garantiza unicidad)
  ✓ Cardinality estimation basada en estadísticas + histogramas
  ✓ Modelo de costos calibrado (seq_page_cost, random_page_cost, cpu_tuple_cost)

Fase 28 — Completitud SQL           (semana 58-60)
  ✓ Niveles de aislamiento: READ COMMITTED, REPEATABLE READ, SERIALIZABLE (SSI)
  ✓ SELECT FOR UPDATE / FOR SHARE / SKIP LOCKED / NOWAIT
  ✓ LOCK TABLE con modos (ACCESS SHARE, ROW EXCLUSIVE, ACCESS EXCLUSIVE)
  ✓ Advisory locks: pg_advisory_lock / pg_try_advisory_lock
  ✓ UNION / UNION ALL / INTERSECT / EXCEPT
  ✓ EXISTS / NOT EXISTS / IN subquery / subqueries correlacionados
  ✓ CASE simple y buscado en SELECT, WHERE, ORDER BY
  ✓ TABLESAMPLE SYSTEM y BERNOULLI con REPEATABLE

Fase 29 — Funciones completas       (semana 61-63)
  ✓ Agregaciones: STRING_AGG, ARRAY_AGG, JSON_AGG, PERCENTILE, MODE, FILTER
  ✓ Ventana: NTILE, PERCENT_RANK, CUME_DIST, FIRST_VALUE, LAST_VALUE, NTH_VALUE
  ✓ Texto: REGEXP_*, LPAD, RPAD, REPEAT, CONCAT_WS, FORMAT, TRANSLATE, INITCAP
  ✓ Fecha: AT TIME ZONE, AGE, TO_CHAR, TO_DATE, EXTRACT, DATE_PART, MAKE_DATE
  ✓ Timezone database completa (tzdata embebida)
  ✓ Matemáticas: SIN/COS/TAN, LOG, SQRT, POWER, GCD, LCM, RANDOM, SETSEED

Fase 30 — Infraestructura pro       (semana 64-66)
  ✓ Índices GIN (arrays, JSONB, trigramas)
  ✓ Índices GiST (rangos, geometría)
  ✓ Índices BRIN (tablas enormes con datos ordenados)
  ✓ Índices Hash (O(1) para igualdad)
  ✓ CREATE INDEX CONCURRENTLY (sin bloquear writes)
  ✓ information_schema completo (tables, columns, constraints, referential_constraints)
  ✓ pg_catalog básico (pg_class, pg_attribute, pg_index)
  ✓ DESCRIBE / SHOW TABLES / SHOW CREATE TABLE / SHOW PROCESSLIST
  ✓ Two-phase commit (PREPARE TRANSACTION / COMMIT PREPARED)
  ✓ DDL Triggers (CREATE EVENT TRIGGER ON ddl_command_end)
  ✓ TABLESPACES (CREATE TABLESPACE, almacenamiento tiered)
  ✓ NOT VALID + VALIDATE CONSTRAINT (constraints sin downtime)
  ✓ GUC: SET/SHOW/ALTER SYSTEM para configuración dinámica
  ✓ pg_stat_activity: ver y cancelar queries en ejecución
  ✓ pg_locks: ver locks activos y bloqueados

Fase 31 — Features finales           (semana 67-69)
  ✓ Cifrado en reposo: AES-256-GCM por página (aes-gcm crate)
  ✓ Data masking: MASK_EMAIL, MASK_PHONE, MASK_CC, PARTIAL, políticas por rol
  ✓ Audit trail: CREATE AUDIT POLICY con logging automático
  ✓ PREPARE / EXECUTE: plan compilado y reutilizable por sesión
  ✓ Estadísticas extendidas: correlación entre columnas (CREATE STATISTICS)
  ✓ FULL OUTER JOIN
  ✓ Custom aggregates: CREATE AGGREGATE con SFUNC + FINALFUNC
  ✓ Custom operators: CREATE OPERATOR
  ✓ Geospatial: POINT, POLYGON, ST_DISTANCE_KM, índice R-Tree (rstar)
  ✓ Query result cache: invalidación automática por tabla
  ✓ Strict mode: sin coerción silenciosa, errores en truncación/fechas inválidas
  ✓ Logical replication: CREATE PUBLICATION + CREATE SUBSCRIPTION
  ✓ Replication slots: WAL retenido hasta que consumidor confirme
  ✓ mTLS: autenticación por certificado cliente + pg_hba.conf equivalente

Fase 32 — Arquitectura final          (semana 70-72)
  ✓ Workspace Rust con 18 crates especializados
  ✓ Trait StorageEngine: MmapStorage, MemoryStorage, EncryptedStorage, FaultInjector
  ✓ Trait Index: BTree, Hash, Gin, Gist, Brin, Hnsw, Fts — intercambiables
  ✓ Engine central con query pipeline completo
  ✓ WAL como event bus: replicación, CDC, cache, triggers, audit
  ✓ Perfiles release optimizados: LTO fat, codegen-units=1, panic=abort

Fase 33 — AI-Native Layer            (semana 73-76)
  ✓ Búsqueda híbrida BM25 + HNSW con RRF (Reciprocal Rank Fusion)
  ✓ Re-ranking con cross-encoder
  ✓ AI_EMBED() con backend Ollama (local) y OpenAI (fallback)
  ✓ AI_CLASSIFY(), AI_EXTRACT(), AI_SUMMARIZE(), AI_TRANSLATE()
  ✓ AI_DETECT_PII() y AI_MASK_PII()
  ✓ Cache de embeddings (mismo texto = mismo vector)
  ✓ VECTOR GENERATED ALWAYS AS (AI_EMBED(col)) STORED
  ✓ RAG Pipeline nativo: CREATE RAG PIPELINE + RAG_QUERY()
  ✓ Feature Store: CREATE FEATURE GROUP + GET_FEATURES() + point-in-time correct
  ✓ Model Store ONNX: CREATE MODEL + PREDICT() + PREDICT_AB()
  ✓ Adaptive indexing: sugerencias automáticas + índices sin uso
  ✓ Text-to-SQL: NL_QUERY(), NL_TO_SQL(), NL_EXPLAIN()
  ✓ Anomaly detection: ANOMALY_SCORE() + CREATE ANOMALY DETECTOR
  ✓ Privacidad diferencial: DP_COUNT, DP_AVG, DP_SUM con presupuesto por rol
  ✓ Data lineage: DATA_LINEAGE() + DATA_LOCATIONS() + GDPR Right to be Forgotten
  ✓ Training dataset tracking: CREATE TRAINING DATASET con snapshots

Fase 34 — Infraestructura distribuida y completitud  (semana 77-80)
  ✓ Sharding horizontal: DISTRIBUTED BY HASH/RANGE/LIST entre N nodos
  ✓ Scatter-gather: ejecutar plan en todos los shards y hacer merge
  ✓ Rebalanceo de shards sin downtime: ALTER TABLE ... REBALANCE SHARDS
  ✓ Hot standby: reads desde réplicas con lag configurable
  ✓ Replica selection: least_lag, round_robin, least_connections
  ✓ Synchronous commit: off, local, remote_write, remote_apply
  ✓ Cascading replication: primary → réplica → sub-réplicas
  ✓ Logical decoding API: pg_logical_slot_get_changes() con plugin JSON
  ✓ Logical output plugins: dbyo_json, wal2json compatible
  ✓ DSN estándar: dbyo://, postgres://, mysql:// + DATABASE_URL env var
  ✓ Extensions system: CREATE/DROP/ALTER EXTENSION + pg_available_extensions
  ✓ Extensiones WASM: CREATE EXTENSION FROM FILE '*.wasm'
  ✓ Online VACUUM sin locks: Normal, Concurrent (sin downtime), Full
  ✓ VACUUM FREEZE: prevención de Transaction ID Wraparound
  ✓ pg_stat_progress_vacuum: ver progreso en tiempo real
  ✓ Parallel DDL: CREATE TABLE AS SELECT WITH PARALLEL N
  ✓ REFRESH MATERIALIZED VIEW CONCURRENTLY WITH PARALLEL N
  ✓ pg_stat_progress_create_index: ver progreso de index build
  ✓ pgbench equivalente: dbyo-bench con escenarios OLTP estándar
  ✓ SQLSTATE codes: códigos de error estándar SQL para compatibilidad con ORMs

Fase 35 — Deployment y DevEx        (semana 81-83)
  ✓ Dockerfile multi-stage: builder Rust + runtime debian-slim mínimo
  ✓ docker-compose.yml: setup completo con volúmenes y healthcheck
  ✓ systemd service: dbyo.service para Linux producción
  ✓ dbyo.toml completo: red, storage, auth, TLS, timeouts, logging, AI, replicación
  ✓ Log levels y rotación: trace/debug/info/warn/error + daily/size rotation
  ✓ dbyo-client crate: SDK oficial Rust con pool, tipos fuertes, transacciones
  ✓ Python package: pip install dbyo-python, API estilo psycopg2
  ✓ GitHub Actions CI: test + clippy + fuzz + bench en cada PR
  ✓ Homebrew formula: brew install dbyo para macOS
  ✓ Guía de performance tuning: parámetros por workload (OLTP, OLAP, mixto)
```

---

## Features de Producto Avanzados

### Vector Similarity Search (pgvector-inspired)

Para IA, embeddings, búsqueda semántica y RAG.

```sql
-- Tipo VECTOR(n) — n dimensiones
CREATE TABLE documentos (
  id        INT PRIMARY KEY,
  titulo    TEXT,
  contenido TEXT,
  embedding VECTOR(1536)   -- OpenAI ada-002 = 1536 dims
);

-- Insertar embedding (generado por modelo de IA)
INSERT INTO documentos (titulo, contenido, embedding)
VALUES ('Intro a Rust', '...', '[0.12, -0.34, 0.89, ...]'::vector);

-- Búsqueda por similitud coseno (más similar primero)
SELECT titulo, embedding <=> '[0.1, 0.2, ...]'::vector AS distancia
FROM documentos
ORDER BY distancia LIMIT 5;

-- Otros operadores
-- <->  distancia euclidiana (L2)
-- <#>  producto punto (inner product)
-- <=>  similitud coseno

-- Índice HNSW — búsqueda aproximada en millones de vectores (~10ms)
CREATE INDEX ON documentos USING hnsw(embedding vector_cosine_ops)
  WITH (m = 16, ef_construction = 64);

-- Índice IVFFlat — más rápido de construir, menos preciso
CREATE INDEX ON documentos USING ivfflat(embedding vector_l2_ops)
  WITH (lists = 100);
```

```rust
struct HnswIndex {
    layers:     Vec<Vec<HnswNode>>,  // grafo jerárquico
    ef_search:  usize,               // candidates en búsqueda
    m:          usize,               // conexiones por nodo
}

impl HnswIndex {
    fn search(&self, query: &[f32], k: usize) -> Vec<(u64, f32)> {
        // 1. Entrar por el layer más alto (pocos nodos)
        // 2. Greedy descent hacia el query
        // 3. En layer 0: beam search con ef_search candidates
        // Retorna (doc_id, distancia) ordenados
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm_a * norm_b)
}
```

Crate: `hnsw_rs = "0.3"` o implementación propia.

---

### Búsqueda Fuzzy — Trigramas (pg_trgm-inspired)

Tolerante a errores tipográficos. Útil para autocompletado y búsquedas de nombres.

```sql
-- Similaridad entre strings (0.0 a 1.0)
SELECT nombre, SIMILARITY(nombre, 'Jhon Doe') AS sim
FROM users
WHERE SIMILARITY(nombre, 'Jhon Doe') > 0.3
ORDER BY sim DESC;
-- encuentra: "John Doe", "Joan Doe", "John Do"

-- Índice GIN de trigramas para SIMILARITY rápido
CREATE INDEX idx_trgm_nombre ON users USING gin(nombre gin_trgm_ops);

-- LIKE con índice trigrama (% al inicio ya no bloquea el índice)
SELECT * FROM products WHERE nombre LIKE '%biblia%';  -- usa el índice trgm

-- Distancia de edición (Levenshtein)
SELECT nombre, LEVENSHTEIN(nombre, 'Rustt') AS edits
FROM tags
WHERE LEVENSHTEIN(nombre, 'Rustt') <= 2
ORDER BY edits;
-- encuentra: "Rust", "Rusty", "Bust"
```

```rust
fn trigrams(s: &str) -> HashSet<[char; 3]> {
    let padded = format!("  {s} ");
    padded.chars().collect::<Vec<_>>()
        .windows(3)
        .map(|w| [w[0], w[1], w[2]])
        .collect()
}

fn trigram_similarity(a: &str, b: &str) -> f32 {
    let ta = trigrams(a);
    let tb = trigrams(b);
    let intersection = ta.intersection(&tb).count() as f32;
    let union = ta.union(&tb).count() as f32;
    if union == 0.0 { 0.0 } else { intersection / union }
}

fn levenshtein(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut dp = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for i in 0..=a.len() { dp[i][0] = i; }
    for j in 0..=b.len() { dp[0][j] = j; }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            dp[i][j] = if a[i-1] == b[j-1] { dp[i-1][j-1] }
                       else { 1 + dp[i-1][j].min(dp[i][j-1]).min(dp[i-1][j-1]) };
        }
    }
    dp[a.len()][b.len()]
}
```

---

### Scheduled Jobs (pg_cron-inspired)

Tareas SQL programadas con expresiones cron. Sin cron del OS, sin scripts externos.

```sql
-- Crear job programado
SELECT cron_schedule(
  'limpiar-logs',           -- nombre del job
  '0 2 * * *',             -- expresión cron: cada día a las 2am
  'DELETE FROM logs WHERE created_at < NOW() - INTERVAL ''30 days'''
);

-- Cada 5 minutos
SELECT cron_schedule('refresh-ventas', '*/5 * * * *',
  'REFRESH MATERIALIZED VIEW ventas_por_hora');

-- Solo días laborables a las 8am
SELECT cron_schedule('reporte-diario', '0 8 * * 1-5',
  'INSERT INTO reportes SELECT NOW(), COUNT(*) FROM orders WHERE DATE(created_at) = TODAY()');

-- Ver jobs activos
SELECT * FROM cron_jobs;

-- Eliminar job
SELECT cron_unschedule('limpiar-logs');

-- Ver historial de ejecuciones
SELECT job_name, ran_at, duration_ms, status, error
FROM cron_history ORDER BY ran_at DESC LIMIT 20;
```

```rust
use tokio_cron_scheduler::{Job, JobScheduler};

struct CronManager {
    scheduler: JobScheduler,
    jobs:      HashMap<String, Uuid>,
}

impl CronManager {
    async fn schedule(&mut self, name: &str, cron: &str, sql: &str, db: Arc<Database>) {
        let sql = sql.to_string();
        let job = Job::new_async(cron, move |_, _| {
            let db = db.clone();
            let sql = sql.clone();
            Box::pin(async move {
                let start = Instant::now();
                match db.execute(&sql).await {
                    Ok(_)  => log_cron_success(name, start.elapsed()),
                    Err(e) => log_cron_error(name, &e),
                }
            })
        }).unwrap();
        let id = self.scheduler.add(job).await.unwrap();
        self.jobs.insert(name.to_string(), id);
    }
}
```

Crate: `tokio-cron-scheduler = "0.11"`.

---

### Foreign Data Wrappers

Consultar fuentes externas como si fueran tablas locales.

```sql
-- Registrar servidor externo
CREATE SERVER ventas_api
  TYPE 'http'
  OPTIONS (url 'https://api.partner.com', format 'json', timeout '5s');

CREATE SERVER otra_bd
  TYPE 'postgres'
  OPTIONS (host 'db2.internal', port '5432', dbname 'analytics');

CREATE SERVER archivos_s3
  TYPE 's3'
  OPTIONS (bucket 'mi-bucket', region 'us-east-1');

-- Tabla virtual que lee del servidor externo
CREATE FOREIGN TABLE ventas_externas (
  id     INT,
  total  DECIMAL,
  fecha  DATE
) SERVER ventas_api OPTIONS (endpoint '/ventas');

CREATE FOREIGN TABLE logs_s3 (
  timestamp TEXT,
  level     TEXT,
  message   TEXT
) SERVER archivos_s3 OPTIONS (prefix 'logs/2026/', format 'jsonl');

-- Usar como tabla normal (JOIN con locales)
SELECT l.nombre, SUM(v.total)
FROM clientes l JOIN ventas_externas v ON l.id = v.client_id
GROUP BY l.nombre;
```

```rust
trait ForeignDataWrapper: Send + Sync {
    fn scan(&self, quals: &[Qual], columns: &[usize]) -> Result<Box<dyn Iterator<Item = Row>>>;
    fn insert(&self, row: &Row) -> Result<()> { Err(DbError::ReadOnly) }
}

struct HttpFdw { url: String, format: FdwFormat }
struct PostgresFdw { conn: tokio_postgres::Client }
struct S3Fdw { bucket: String, client: aws_sdk_s3::Client }

impl ForeignDataWrapper for HttpFdw {
    fn scan(&self, quals: &[Qual], _: &[usize]) -> Result<Box<dyn Iterator<Item = Row>>> {
        let params = quals_to_query_params(quals);
        let resp: Vec<serde_json::Value> = reqwest::blocking::get(
            format!("{}{}", self.url, params)
        )?.json()?;
        Ok(Box::new(resp.into_iter().map(json_to_row)))
    }
}
```

---

### Multi-Database y Schema Namespacing

```sql
-- Múltiples bases de datos en el mismo servidor
CREATE DATABASE ventas;
CREATE DATABASE analytics;
CREATE DATABASE myapp_test;
DROP DATABASE myapp_test;

-- Cambiar de BD activa
USE ventas;
SHOW DATABASES;

-- Schemas dentro de una BD (namespacing)
CREATE SCHEMA contabilidad;
CREATE SCHEMA inventario;
CREATE SCHEMA public;   -- default

-- Tablas dentro de schemas
CREATE TABLE contabilidad.facturas (id INT, total DECIMAL);
CREATE TABLE inventario.productos  (id INT, stock INT);

-- Search path (schemas por defecto para resolución de nombres)
SET search_path = contabilidad, public;
SELECT * FROM facturas;  -- busca contabilidad.facturas primero

-- Cross-database query
SELECT v.id, a.score
FROM ventas.public.orders v
JOIN analytics.public.scores a ON v.id = a.order_id;
```

---

### Schema Migrations CLI

```bash
# Inicializar sistema de migrations
dbyo migrate init

# Crear nueva migration
dbyo migrate new "agregar_columna_activo_a_users"
# → crea migrations/0001_agregar_columna_activo_a_users.sql

# Aplicar migrations pendientes
dbyo migrate up

# Revertir última migration
dbyo migrate down

# Estado actual
dbyo migrate status
# 0001_init                    ✓ applied  2026-03-01
# 0002_add_roles               ✓ applied  2026-03-10
# 0003_agregar_columna_activo  ✗ pending

# Ir a versión específica
dbyo migrate to 0002
```

```sql
-- Tabla interna de control
CREATE TABLE _dbyo_migrations (
  version    INT PRIMARY KEY,
  name       TEXT NOT NULL,
  applied_at TIMESTAMP DEFAULT NOW(),
  checksum   TEXT,      -- hash del archivo SQL (detecta modificaciones)
  duration_ms INT
);
```

```rust
// migrations/0001_init.sql  →  migrations/0001_init.down.sql
struct Migration {
    version:  u32,
    name:     String,
    up_sql:   String,   // aplicar
    down_sql: String,   // revertir
    checksum: String,
}

async fn migrate_up(db: &Database, target: Option<u32>) -> Result<()> {
    let applied = db.get_applied_migrations().await?;
    let all = load_migration_files("./migrations")?;
    for m in all.iter().filter(|m| !applied.contains(&m.version)) {
        if target.map(|t| m.version > t).unwrap_or(false) { break; }
        db.execute(&m.up_sql).await?;
        db.record_migration(&m).await?;
        println!("✓ Applied {}: {}", m.version, m.name);
    }
    Ok(())
}
```

---

## Retrocompatibilidad con SQLite, MySQL y PostgreSQL

### Leer archivos SQLite directamente

```sql
-- Montar archivo SQLite como fuente externa (sin copiar datos)
ATTACH '/path/to/biblia.sqlite' AS src USING sqlite;

-- Consultar directamente
SELECT book, chapter, verse, text FROM src.verses WHERE book = 'Juan';

-- Migrar a nuestra BD
INSERT INTO verses SELECT * FROM src.verses;

-- Detach cuando terminas
DETACH src;
```

```rust
// Leer el formato binario de SQLite directamente
// Magic: "SQLite format 3\000" en los primeros 16 bytes
struct SqliteFileReader {
    mmap:      Mmap,
    page_size: u16,   // bytes 16-17 del header
}

impl SqliteFileReader {
    fn tables(&self) -> Vec<String> {
        // Leer sqlite_master en página 1
        self.read_btree_table("sqlite_master")
            .filter(|r| r.get_text("type") == "table")
            .map(|r| r.get_text("name").to_string())
            .collect()
    }

    fn scan_table(&self, name: &str) -> impl Iterator<Item = Row> + '_ {
        let root_page = self.find_root_page(name);
        self.traverse_btree(root_page)
    }
}

// Alternativa usando rusqlite como backend de lectura
use rusqlite::Connection;
fn read_sqlite(path: &str) -> Result<impl ForeignDataWrapper> {
    Ok(SqliteFdw { conn: Connection::open(path)? })
}
```

### Migración desde MySQL

```bash
# CLI — migración completa
dbyo migrate from-mysql \
  --host     localhost  \
  --port     3306       \
  --user     root       \
  --password secret     \
  --database myapp      \
  --target   myapp      \
  --batch-size 1000

# Lo que hace internamente:
# 1. Lee INFORMATION_SCHEMA.TABLES / COLUMNS / INDEXES / KEY_COLUMN_USAGE
# 2. Traduce tipos MySQL → tipos propios (TINYINT→INT, DATETIME→TIMESTAMP, etc.)
# 3. Recrea tablas con nuestro DDL
# 4. Copia datos en batches de 1000 filas
# 5. Recrea índices y FKs
# 6. Verifica checksums COUNT(*) por tabla
```

```rust
use mysql_async::{Pool, prelude::*};

async fn migrate_from_mysql(opts: MysqlMigrateOpts, db: Arc<Database>) -> Result<Stats> {
    let pool = Pool::new(opts.conn_str());
    let mut conn = pool.get_conn().await?;

    // 1. Leer schema
    let tables: Vec<String> = conn
        .query("SHOW TABLES").await?;

    for table in &tables {
        // Traducir CREATE TABLE de MySQL a nuestro DDL
        let (_, create_sql): (String, String) = conn
            .query_first(format!("SHOW CREATE TABLE `{table}`")).await?.unwrap();
        let our_ddl = translate_mysql_ddl(&create_sql)?;
        db.execute(&our_ddl).await?;

        // Copiar datos en batches
        let mut offset = 0u64;
        loop {
            let rows: Vec<mysql_async::Row> = conn
                .query(format!("SELECT * FROM `{table}` LIMIT 1000 OFFSET {offset}"))
                .await?;
            if rows.is_empty() { break; }
            db.bulk_insert(table, mysql_rows_to_rows(&rows)?).await?;
            offset += 1000;
        }
    }
    Ok(Stats { tables: tables.len(), .. })
}
```

Crate: `mysql_async = "0.34"`.

### Migración desde PostgreSQL

```bash
dbyo migrate from-postgres \
  --conn "postgresql://user:pass@host:5432/mydb" \
  --schema public \
  --target mydb_migrated

# También acepta pg_dump:
dbyo source dump.sql   # pg_dump --format=plain
```

```rust
use tokio_postgres::NoTls;

async fn migrate_from_postgres(conn_str: &str, db: Arc<Database>) -> Result<Stats> {
    let (client, connection) = tokio_postgres::connect(conn_str, NoTls).await?;
    tokio::spawn(connection);

    // Leer tablas del schema public
    let tables = client.query(
        "SELECT tablename FROM pg_tables WHERE schemaname = $1", &[&"public"]
    ).await?;

    for row in &tables {
        let table: &str = row.get(0);

        // Leer columnas con tipos
        let cols = client.query(
            "SELECT column_name, data_type, is_nullable, column_default
             FROM information_schema.columns
             WHERE table_name = $1 ORDER BY ordinal_position", &[&table]
        ).await?;

        let ddl = build_create_table(table, &cols);
        db.execute(&ddl).await?;

        // Streaming de datos con COPY (más rápido que SELECT)
        let copy_out = client
            .copy_out(&format!("COPY {table} TO STDOUT WITH (FORMAT binary)"))
            .await?;
        db.bulk_insert_binary_stream(table, copy_out).await?;
    }
    Ok(Stats::default())
}
```

### PostgreSQL Wire Protocol (puerto 5432)

Los clientes PostgreSQL se conectan sin cambios: `psql`, `psycopg2`, `pgx`, `pg` (Node), `npgsql` (.NET).

```
Puerto 3306 → MySQL wire protocol    (PHP PDO, Python MySQLdb, mysql2)
Puerto 5432 → PostgreSQL wire protocol (psql, psycopg2, pgx, npgsql)
```

```rust
// Startup message de PostgreSQL (diferente al handshake MySQL)
struct PgStartup {
    protocol_version: u32,   // 196608 = versión 3.0
    parameters: HashMap<String, String>,  // user, database, application_name...
}

async fn handle_pg_client(stream: TcpStream, engine: Arc<Engine>) {
    let startup = read_pg_startup(&stream).await;
    // Autenticación: MD5 o Scram-SHA-256
    send_auth_request(&stream, AuthMethod::ScramSha256).await;
    let creds = read_auth_response(&stream).await;
    verify_credentials(&creds, &engine).await;
    send_ready_for_query(&stream).await;

    loop {
        match read_pg_message(&stream).await {
            PgMessage::Query(sql)    => handle_simple_query(sql, &stream, &engine).await,
            PgMessage::Parse(stmt)   => handle_extended_query(stmt, &stream, &engine).await,
            PgMessage::Terminate     => break,
        }
    }
}

// Servidor escuchando en ambos puertos simultáneamente
#[tokio::main]
async fn main() {
    let engine = Arc::new(Engine::new());
    tokio::join!(
        listen_mysql(    "0.0.0.0:3306", engine.clone()),
        listen_postgres( "0.0.0.0:5432", engine.clone()),
    );
}
```

---

## Completitud para BD Profesional

### Query Optimizer Real — Cost-Based

El mayor gap vs PostgreSQL. Sin esto, el planner elige planes subóptimos.

```
Lo que tenemos: elegir índice vs full scan (1 tabla)
Lo que necesitamos: optimizar el plan completo con N tablas y N joins
```

```rust
// Join ordering con programación dinámica (igual que PostgreSQL)
struct QueryOptimizer {
    stats:    Arc<StatsCatalog>,
    cost_model: CostModel,
}

impl QueryOptimizer {
    // Para N tablas, evalúa 2^N subconjuntos posibles de join
    fn find_best_join_order(&self, tables: &[TableRef]) -> JoinPlan {
        let n = tables.len();
        // dp[S] = mejor plan para el subconjunto S de tablas
        let mut dp: HashMap<BitSet, (JoinPlan, f64)> = HashMap::new();

        // Inicializar con tablas individuales
        for (i, t) in tables.iter().enumerate() {
            let bs = BitSet::single(i);
            let scan = self.best_scan(t);
            let cost = self.cost_model.scan_cost(t, &self.stats);
            dp.insert(bs, (scan, cost));
        }

        // Combinar subconjuntos de mayor tamaño
        for size in 2..=n {
            for subset in subsets_of_size(n, size) {
                let mut best: Option<(JoinPlan, f64)> = None;
                for (left, right) in partitions(subset) {
                    if let (Some((lp, lc)), Some((rp, rc))) = (dp.get(&left), dp.get(&right)) {
                        for join_type in [HashJoin, NestedLoop, MergeJoin] {
                            let cost = lc + rc + self.cost_model.join_cost(&join_type, lp, rp, &self.stats);
                            if best.as_ref().map(|(_, c)| cost < *c).unwrap_or(true) {
                                best = Some((JoinPlan::new(join_type, lp.clone(), rp.clone()), cost));
                            }
                        }
                    }
                }
                dp.insert(subset, best.unwrap());
            }
        }
        dp[&BitSet::all(n)].0.clone()
    }

    // Predicate pushdown: mover filtros cerca de los datos
    fn pushdown_predicates(&self, plan: &mut LogicalPlan) {
        // WHERE age > 25 en un JOIN → moverlo al scan de la tabla
        // Reduce filas antes del join → join más barato
    }

    // Subquery unnesting: convertir subquery a JOIN
    fn unnest_subquery(&self, subquery: &SubqueryExpr) -> Option<JoinPlan> {
        // SELECT * FROM t WHERE id IN (SELECT t2.fk FROM t2)
        // → SELECT t.* FROM t JOIN t2 ON t.id = t2.fk (más rápido)
    }
}
```

---

### Niveles de Aislamiento de Transacciones

```sql
-- Configurar por sesión o transacción
SET TRANSACTION ISOLATION LEVEL READ COMMITTED;
SET TRANSACTION ISOLATION LEVEL REPEATABLE READ;
SET TRANSACTION ISOLATION LEVEL SERIALIZABLE;

BEGIN ISOLATION LEVEL SERIALIZABLE;
  -- operaciones...
COMMIT;

SHOW transaction_isolation;
```

```
READ UNCOMMITTED → ve dirty reads (filas no commiteadas)
                   útil para analytics donde exactitud < velocidad

READ COMMITTED   → ve solo commits (default MySQL)
                   cada statement tiene su propio snapshot

REPEATABLE READ  → snapshot fijo al inicio de la transacción
                   no ve nuevas filas insertadas por otros (phantom reads posibles)

SERIALIZABLE     → SSI: Serializable Snapshot Isolation
                   detecta y aborta transacciones conflictivas
                   equivalente a ejecución serial sin sacrificar concurrencia
```

```rust
#[derive(Clone, Copy, PartialEq)]
enum IsolationLevel {
    ReadUncommitted,
    ReadCommitted,
    RepeatableRead,
    Serializable,   // SSI con detección de ciclos read-write
}

// SSI: rastrear dependencias read-write entre transacciones
struct SsiTracker {
    // Grafo de dependencias: Tx → lista de Txs que leyeron datos que Tx escribió
    rw_deps: HashMap<TxnId, Vec<TxnId>>,
}

impl SsiTracker {
    // Si hay ciclo en el grafo → serialization failure → abortar
    fn check_for_cycle(&self, txn: TxnId) -> bool {
        // DFS buscando camino de vuelta a txn
        let mut visited = HashSet::new();
        self.dfs(txn, txn, &mut visited)
    }
}
```

---

### SELECT FOR UPDATE / FOR SHARE — Bloqueos Explícitos

```sql
-- Bloquear filas para escritura (nadie más puede modificarlas)
SELECT * FROM products WHERE id = 1 FOR UPDATE;

-- Bloquear filas para lectura (otros pueden leer pero no escribir)
SELECT * FROM products WHERE id = 1 FOR SHARE;

-- No esperar si hay lock (lanzar error inmediatamente)
SELECT * FROM products WHERE id = 1 FOR UPDATE NOWAIT;

-- Saltar filas bloqueadas (para job queues)
SELECT * FROM jobs WHERE status = 'pending' LIMIT 1 FOR UPDATE SKIP LOCKED;

-- Bloqueo de tabla completa
LOCK TABLE orders IN EXCLUSIVE MODE;
LOCK TABLE orders IN ACCESS SHARE MODE;

-- Advisory locks (nivel aplicación)
SELECT pg_advisory_lock(12345);
SELECT pg_advisory_unlock(12345);
SELECT pg_try_advisory_lock(12345);   -- non-blocking, retorna bool
```

```rust
#[derive(Clone, PartialEq)]
enum LockMode {
    AccessShare,       // SELECT
    RowShare,          // SELECT FOR SHARE
    RowExclusive,      // INSERT, UPDATE, DELETE
    ShareUpdateExclusive, // VACUUM, CREATE INDEX CONCURRENTLY
    Share,             // CREATE INDEX
    ShareRowExclusive, // CREATE TRIGGER
    Exclusive,         // bloquea casi todo
    AccessExclusive,   // ALTER TABLE, DROP TABLE
}

struct LockManager {
    // row_id → (TxnId, LockMode)
    row_locks:   DashMap<(TableId, RowId), (TxnId, LockMode)>,
    table_locks: DashMap<TableId, (TxnId, LockMode)>,
    advisory:    DashMap<i64, TxnId>,
}

impl LockManager {
    async fn acquire_row(&self, row: RowId, mode: LockMode, txn: TxnId, nowait: bool) -> Result<()> {
        if self.row_locks.contains_key(&row) {
            if nowait { return Err(DbError::LockNotAvailable); }
            self.wait_for_lock(row, txn).await?;
        }
        self.row_locks.insert(row, (txn, mode));
        Ok(())
    }
}
```

---

### UNION / INTERSECT / EXCEPT — Operaciones de Conjunto

```sql
-- UNION: combinar resultados (elimina duplicados)
SELECT id, nombre FROM clientes_activos
UNION
SELECT id, nombre FROM clientes_vip;

-- UNION ALL: combinar sin eliminar duplicados (más rápido)
SELECT product_id FROM orders_2024
UNION ALL
SELECT product_id FROM orders_2025;

-- INTERSECT: filas que aparecen en AMBOS resultados
SELECT user_id FROM compradores
INTERSECT
SELECT user_id FROM suscriptores;

-- EXCEPT: filas del primero que NO están en el segundo
SELECT user_id FROM todos_los_usuarios
EXCEPT
SELECT user_id FROM usuarios_bloqueados;

-- Combinable con ORDER BY y LIMIT
(SELECT nombre FROM tabla_a ORDER BY nombre LIMIT 10)
UNION ALL
(SELECT nombre FROM tabla_b ORDER BY nombre LIMIT 10)
ORDER BY nombre LIMIT 10;
```

```rust
enum SetOperation { Union, UnionAll, Intersect, Except }

fn execute_set_op(left: Vec<Row>, right: Vec<Row>, op: SetOperation) -> Vec<Row> {
    match op {
        UnionAll    => { let mut r = left; r.extend(right); r }
        Union       => dedup(execute_set_op(left, right, UnionAll))
        Intersect   => {
            let set: HashSet<_> = right.iter().collect();
            left.into_iter().filter(|r| set.contains(r)).collect()
        }
        Except      => {
            let set: HashSet<_> = right.iter().collect();
            left.into_iter().filter(|r| !set.contains(r)).collect()
        }
    }
}
```

---

### Subqueries Correlacionados y EXISTS

```sql
-- EXISTS: verdadero si la subquery retorna al menos 1 fila
SELECT * FROM users u
WHERE EXISTS (
  SELECT 1 FROM orders o WHERE o.user_id = u.id AND o.total > 1000
);

-- NOT EXISTS
SELECT * FROM products p
WHERE NOT EXISTS (
  SELECT 1 FROM order_items oi WHERE oi.product_id = p.id
);

-- IN con subquery
SELECT * FROM orders
WHERE user_id IN (SELECT id FROM users WHERE role = 'vip');

-- Subquery correlacionado en SELECT
SELECT u.nombre,
  (SELECT COUNT(*) FROM orders o WHERE o.user_id = u.id) AS total_pedidos,
  (SELECT SUM(total) FROM orders o WHERE o.user_id = u.id) AS monto_total
FROM users u;

-- Subquery en FROM (tabla derivada)
SELECT avg_por_dept.dept, AVG(avg_por_dept.salario)
FROM (
  SELECT dept, AVG(salary) AS salario FROM employees GROUP BY dept
) AS avg_por_dept
GROUP BY avg_por_dept.dept;
```

---

### Tipos de Índice Adicionales

```sql
-- GIN: Generalized Inverted Index — para arrays y JSONB
CREATE INDEX idx_tags    ON posts USING gin(tags);
CREATE INDEX idx_jsonb   ON events USING gin(data jsonb_path_ops);
CREATE INDEX idx_trgm    ON users USING gin(nombre gin_trgm_ops);   -- trigramas

SELECT * FROM posts WHERE tags @> ARRAY['rust', 'database'];
SELECT * FROM events WHERE data @> '{"type": "click"}';

-- GiST: Generalized Search Tree — para rangos y geometría
CREATE INDEX idx_periodo ON reservas USING gist(periodo);
CREATE INDEX idx_punto   ON lugares  USING gist(coordenadas);

SELECT * FROM reservas WHERE periodo && '[2026-03-21, 2026-03-22)'::tsrange;

-- BRIN: Block Range Index — para tablas enormes con datos ordenados naturalmente
-- 1000x más pequeño que B+ Tree, ideal para logs de millones de filas
CREATE INDEX idx_log_time ON logs USING brin(created_at) WITH (pages_per_range = 128);
-- Solo guarda (min, max) por rango de páginas — muy compacto

-- Hash: O(1) para igualdad exacta
CREATE INDEX idx_token ON sessions USING hash(token);
SELECT * FROM sessions WHERE token = 'abc123';  -- 1 lookup, sin árbol

-- Concurrent: crear índice sin bloquear writes
CREATE INDEX CONCURRENTLY idx_email ON users(email);
-- Escanea la tabla en background, acepta writes durante la construcción
```

```rust
enum IndexType {
    BTree,        // rangos, ordenamiento, >, <, BETWEEN
    Hash,         // solo igualdad = — más rápido para ese caso
    Gin,          // arrays, JSONB, FTS — búsqueda dentro de contenido
    Gist,         // rangos, tipos geométricos, custom
    Brin,         // tablas grandes ordenadas — mínimo espacio
    Hnsw,         // vector similarity
    Inverted,     // FTS (ya implementado)
}
```

---

### CASE Expressions

```sql
-- CASE simple
SELECT nombre,
  CASE estado
    WHEN 'A' THEN 'Activo'
    WHEN 'I' THEN 'Inactivo'
    WHEN 'S' THEN 'Suspendido'
    ELSE 'Desconocido'
  END AS estado_texto
FROM users;

-- CASE buscado (condiciones arbitrarias)
SELECT producto, precio,
  CASE
    WHEN precio > 10000 THEN 'Premium'
    WHEN precio > 1000  THEN 'Estándar'
    WHEN precio > 100   THEN 'Económico'
    ELSE 'Básico'
  END AS categoria_precio
FROM products;

-- CASE en ORDER BY
SELECT * FROM tasks
ORDER BY
  CASE prioridad
    WHEN 'critica' THEN 1
    WHEN 'alta'    THEN 2
    WHEN 'media'   THEN 3
    ELSE 4
  END;

-- CASE en agregaciones
SELECT
  COUNT(CASE WHEN estado = 'activo'   THEN 1 END) AS activos,
  COUNT(CASE WHEN estado = 'inactivo' THEN 1 END) AS inactivos
FROM users;
```

---

### Funciones de Agregación Avanzadas

```sql
-- Concatenación
SELECT dept, STRING_AGG(nombre, ', ' ORDER BY nombre) AS empleados
FROM employees GROUP BY dept;

-- Array
SELECT user_id, ARRAY_AGG(product_id ORDER BY created_at DESC) AS historial
FROM orders GROUP BY user_id;

-- JSON
SELECT JSON_AGG(row_to_json(u.*)) AS usuarios FROM users u;
SELECT JSON_OBJECT_AGG(key, value) FROM config;

-- Estadísticas
SELECT
  PERCENTILE_CONT(0.50) WITHIN GROUP (ORDER BY salary) AS mediana,
  PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY salary) AS p95,
  PERCENTILE_DISC(0.50) WITHIN GROUP (ORDER BY salary) AS mediana_discreta,
  MODE() WITHIN GROUP (ORDER BY departamento)          AS dept_mas_comun
FROM employees;

-- Filtro condicional en agregación
SELECT
  COUNT(*) FILTER (WHERE estado = 'error')   AS errores,
  COUNT(*) FILTER (WHERE estado = 'ok')      AS exitosos,
  AVG(duracion_ms) FILTER (WHERE estado = 'ok') AS avg_ok
FROM requests;
```

```rust
enum AggFunc {
    Count, Sum, Avg, Min, Max,
    StringAgg { separator: String, order: Vec<(Expr, Order)> },
    ArrayAgg  { order: Vec<(Expr, Order)> },
    JsonAgg, JsonObjectAgg,
    PercentileCont(f64), PercentileDisc(f64),
    Mode,
    StdDev, Variance,
    BoolAnd, BoolOr,
    BitAnd, BitOr, BitXor,
}

struct AggState {
    func:   AggFunc,
    filter: Option<Expr>,   // FILTER (WHERE ...)
    rows:   Vec<Value>,
}
```

---

### Funciones de Ventana Adicionales

```sql
SELECT nombre, dept, salary,
  NTILE(4)       OVER (ORDER BY salary)        AS cuartil,       -- 1,2,3,4
  PERCENT_RANK() OVER (ORDER BY salary)        AS percentil,     -- 0.0 a 1.0
  CUME_DIST()    OVER (ORDER BY salary)        AS dist_acum,     -- 0.0 a 1.0
  FIRST_VALUE(salary) OVER (PARTITION BY dept ORDER BY salary DESC) AS max_dept,
  LAST_VALUE(salary)  OVER (PARTITION BY dept ORDER BY salary
                            ROWS BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) AS min_dept,
  NTH_VALUE(salary, 2) OVER (PARTITION BY dept ORDER BY salary DESC) AS segundo_mayor
FROM employees;
```

---

### Funciones de Texto Completas

```sql
-- Expresiones regulares
SELECT REGEXP_MATCH('2026-03-21', '(\d{4})-(\d{2})-(\d{2})');   -- ['2026','03','21']
SELECT REGEXP_REPLACE('Hola Mundo', '[aeiou]', '*', 'gi');        -- 'H*l* M*nd*'
SELECT REGEXP_SPLIT_TO_TABLE('a,b,c', ',');                       -- 3 filas: a, b, c
SELECT nombre ~ '^[A-Z]';    -- coincide con regex (case-sensitive)
SELECT nombre ~* '^[a-z]';   -- coincide con regex (case-insensitive)

-- Padding y formato
SELECT LPAD(codigo::TEXT, 8, '0');         -- '42' → '00000042'
SELECT RPAD(nombre, 20, '.');              -- 'Ana.........'
SELECT REPEAT('ab', 3);                   -- 'ababab'
SELECT CONCAT_WS(', ', nombre, apellido, ciudad); -- 'Ana, García, Bogotá' (ignora NULLs)
SELECT FORMAT('Hola %s, tienes %s años', nombre, edad::TEXT);

-- Manipulación
SELECT OVERLAY(texto PLACING 'NUEVO' FROM 3 FOR 5);
SELECT TRANSLATE(texto, 'aeiou', 'AEIOU');       -- reemplazar caracteres uno a uno
SELECT INITCAP('hola mundo');                    -- 'Hola Mundo'
SELECT REVERSE('Hola');                          -- 'aloH'
SELECT LEFT(texto, 50);                          -- primeros 50 chars
SELECT RIGHT(texto, 50);                         -- últimos 50 chars
SELECT SPLIT_PART('a,b,c', ',', 2);             -- 'b'
SELECT STRPOS(texto, 'buscar');                  -- posición (0 si no existe)
SELECT MD5(texto);                               -- hash MD5 como hex string
SELECT ENCODE(bytea_col, 'base64');              -- codificar en base64
SELECT DECODE('SGVsbG8=', 'base64');             -- decodificar de base64
```

```rust
fn register_string_functions(registry: &mut FunctionRegistry) {
    registry.register("regexp_match",   |args| { /* regex match */ });
    registry.register("regexp_replace", |args| { /* regex replace */ });
    registry.register("lpad",           |args| { /* left pad */ });
    registry.register("rpad",           |args| { /* right pad */ });
    registry.register("repeat",         |args| { /* repeat string */ });
    registry.register("concat_ws",      |args| { /* concat with separator, skip nulls */ });
    registry.register("format",         |args| { /* printf-style format */ });
    registry.register("translate",      |args| { /* char-by-char replace */ });
    registry.register("initcap",        |args| { /* capitalize words */ });
    registry.register("md5",            |args| { /* MD5 hash */ });
    registry.register("encode",         |args| { /* base64/hex encode */ });
    registry.register("decode",         |args| { /* base64/hex decode */ });
}
```

Crate: `regex = "1"` para REGEXP_*.

---

### Funciones de Fecha y Timezone Completas

```sql
-- Timezone completo
SELECT NOW() AT TIME ZONE 'America/Bogota';
SELECT NOW() AT TIME ZONE 'America/New_York';
SELECT '2026-03-21 10:00:00'::TIMESTAMP AT TIME ZONE 'UTC' AT TIME ZONE 'America/Bogota';

-- Extracción
SELECT EXTRACT(YEAR   FROM created_at) AS año,
       EXTRACT(MONTH  FROM created_at) AS mes,
       EXTRACT(DOW    FROM created_at) AS dia_semana,  -- 0=domingo
       EXTRACT(WEEK   FROM created_at) AS semana_del_año,
       EXTRACT(EPOCH  FROM created_at) AS unix_timestamp;

SELECT DATE_PART('hour', created_at);
SELECT DATE_TRUNC('month', created_at);  -- primer día del mes
SELECT DATE_TRUNC('week',  created_at);  -- lunes de esa semana

-- Aritmética de fechas
SELECT AGE(fecha_nacimiento);              -- '25 years 3 months 12 days'
SELECT AGE(CURRENT_DATE, fecha_nacimiento);
SELECT fecha_nacimiento + INTERVAL '1 year';
SELECT created_at + INTERVAL '30 days';
SELECT NOW() - created_at AS antigüedad;

-- Formato
SELECT TO_CHAR(created_at, 'DD/MM/YYYY HH24:MI:SS');
SELECT TO_CHAR(precio,     'FM$999,999.00');
SELECT TO_DATE('21/03/2026', 'DD/MM/YYYY');
SELECT TO_TIMESTAMP('21/03/2026 10:30', 'DD/MM/YYYY HH24:MI');
SELECT TO_NUMBER('1,234.56', '9,999.99');

-- Utilidades
SELECT CURRENT_DATE, CURRENT_TIME, CURRENT_TIMESTAMP, NOW();
SELECT MAKE_DATE(2026, 3, 21);
SELECT MAKE_TIMESTAMP(2026, 3, 21, 10, 30, 0);
SELECT MAKE_INTERVAL(years => 1, months => 2, days => 3);
SELECT ISFINITE(TIMESTAMP '2026-03-21');
SELECT CLOCK_TIMESTAMP();   -- tiempo real (no congela en la transacción)
```

```rust
use tz::TimeZone;

struct TimezoneDatabase {
    // tz database embebida (tzdata crate)
    zones: HashMap<String, TimeZone>,
}

// Carga automática desde el sistema o embebida
fn load_tzdb() -> TimezoneDatabase {
    TimezoneDatabase {
        zones: tzdata::timezones()
            .map(|(name, tz)| (name.to_string(), tz))
            .collect()
    }
}
```

Crate: `tzdata = "0.1"` (tz database embebida, portable), `chrono-tz = "0.9"`.

---

### Funciones Matemáticas Completas

```sql
-- Básicas
SELECT ABS(-5), SIGN(-3.14), MOD(10, 3), DIV(10, 3);
SELECT CEIL(4.2), FLOOR(4.8), ROUND(4.567, 2), TRUNC(4.999, 1);

-- Potencias y logaritmos
SELECT SQRT(16), CBRT(27);                   -- raíz cuadrada, cúbica
SELECT POWER(2, 10);                         -- 1024
SELECT LOG(2, 1024);                         -- log base 2 → 10
SELECT LOG10(1000), LN(2.718);

-- Trigonometría (en radianes)
SELECT SIN(PI()/2), COS(0), TAN(PI()/4);
SELECT ASIN(1), ACOS(1), ATAN(1), ATAN2(1, 1);
SELECT DEGREES(PI()), RADIANS(180);

-- Estadística
SELECT RANDOM();                             -- 0.0 a 1.0
SELECT SETSEED(0.5);                         -- semilla determinística
SELECT GCD(12, 8), LCM(4, 6);

-- Bits
SELECT BIT_COUNT(255::BIGINT);              -- número de bits en 1
SELECT 255 & 15, 255 | 16, 255 # 240;      -- AND, OR, XOR
SELECT 1 << 4, 256 >> 2;                   -- shift left, right

-- Redondeo financiero
SELECT ROUND(2.5),  ROUND(3.5);            -- banker's rounding (IEEE 754)
SELECT ROUND_HALF_UP(2.5), ROUND_HALF_UP(3.5);  -- rounding tradicional
```

```rust
fn register_math_functions(registry: &mut FunctionRegistry) {
    use std::f64::consts::PI;
    registry.register("abs",     |args| Ok(args[0].abs()));
    registry.register("sqrt",    |args| Ok(args[0].as_f64()?.sqrt().into()));
    registry.register("cbrt",    |args| Ok(args[0].as_f64()?.cbrt().into()));
    registry.register("power",   |args| Ok(args[0].as_f64()?.powf(args[1].as_f64()?).into()));
    registry.register("log",     |args| Ok((args[1].as_f64()?.ln() / args[0].as_f64()?.ln()).into()));
    registry.register("sin",     |args| Ok(args[0].as_f64()?.sin().into()));
    registry.register("cos",     |args| Ok(args[0].as_f64()?.cos().into()));
    registry.register("random",  |_|    Ok(rand::random::<f64>().into()));
    registry.register("gcd",     |args| Ok(gcd(args[0].as_i64()?, args[1].as_i64()?).into()));
    registry.register("pi",      |_|    Ok(PI.into()));
}

fn gcd(a: i64, b: i64) -> i64 { if b == 0 { a.abs() } else { gcd(b, a % b) } }
```

Crate: `rand = "0.8"` para RANDOM().

---

### Information Schema y System Catalogs

```sql
-- Estándar SQL — necesario para ORMs (Hibernate, SQLAlchemy, Prisma, etc.)
SELECT * FROM information_schema.tables
  WHERE table_schema = 'public' AND table_type = 'BASE TABLE';

SELECT column_name, data_type, is_nullable, column_default, character_maximum_length
FROM information_schema.columns
WHERE table_name = 'users' ORDER BY ordinal_position;

SELECT * FROM information_schema.table_constraints WHERE table_name = 'orders';
SELECT * FROM information_schema.referential_constraints;
SELECT * FROM information_schema.key_column_usage;

-- pg_catalog (para compatibilidad con herramientas PostgreSQL)
SELECT oid, relname, relkind FROM pg_class WHERE relnamespace = 'public'::regnamespace;
SELECT attname, atttypid, attnotnull FROM pg_attribute WHERE attrelid = 'users'::regclass;

-- Comandos rápidos de introspección
DESCRIBE users;                  -- columnas y tipos
SHOW TABLES;                     -- listar tablas
SHOW TABLES LIKE 'order%';
SHOW CREATE TABLE users;         -- DDL completo de la tabla
SHOW INDEXES FROM users;
SHOW PROCESSLIST;                -- queries en ejecución
SHOW VARIABLES LIKE 'max%';     -- variables de configuración
```

```rust
// Vistas del sistema implementadas como tablas virtuales
struct InformationSchema {
    engine: Arc<Engine>,
}

impl InformationSchema {
    fn tables(&self) -> Vec<Row> {
        self.engine.catalog().tables().map(|t| Row::from([
            ("table_catalog",   Value::Text(self.engine.db_name().into())),
            ("table_schema",    Value::Text(t.schema.into())),
            ("table_name",      Value::Text(t.name.into())),
            ("table_type",      Value::Text("BASE TABLE".into())),
        ])).collect()
    }

    fn columns(&self, table: &str) -> Vec<Row> {
        self.engine.catalog().columns(table)
            .enumerate()
            .map(|(i, col)| Row::from([
                ("ordinal_position", Value::Int(i as i32 + 1)),
                ("column_name",      Value::Text(col.name.into())),
                ("data_type",        Value::Text(col.type_name().into())),
                ("is_nullable",      Value::Text(if col.nullable { "YES" } else { "NO" }.into())),
            ])).collect()
    }
}
```

---

### TABLESAMPLE — Muestreo Aleatorio Eficiente

```sql
-- SYSTEM: muestreo por páginas (más rápido, menos uniforme)
SELECT * FROM orders TABLESAMPLE SYSTEM(10);     -- ~10% de páginas

-- BERNOULLI: muestreo por filas (más lento, más uniforme)
SELECT * FROM orders TABLESAMPLE BERNOULLI(5);   -- ~5% de filas

-- Reproducible con semilla
SELECT * FROM orders TABLESAMPLE SYSTEM(10) REPEATABLE (42);

-- Mucho más rápido que ORDER BY RANDOM() LIMIT N:
-- TABLESAMPLE: O(páginas × fracción) — no lee toda la tabla
-- ORDER BY RANDOM(): O(n log n)     — lee y ordena toda la tabla
```

```rust
fn tablesample_system(table: &Table, pct: f64, seed: Option<u64>) -> impl Iterator<Item=Row> {
    let rng = seed.map(StdRng::seed_from_u64).unwrap_or_else(StdRng::from_entropy);
    table.pages().filter(move |_| rng.gen_bool(pct / 100.0)).flat_map(|p| p.rows())
}
```

---

### Two-Phase Commit (2PC) — Transacciones Distribuidas

```sql
-- Para coordinar transacciones entre múltiples BDs o servicios
BEGIN;
  UPDATE accounts SET balance = balance - 100 WHERE id = 1;
PREPARE TRANSACTION 'tx_pago_001';   -- fase 1: preparar (no commita aún)

-- ... verificar que el servicio externo también está listo ...

COMMIT PREPARED   'tx_pago_001';     -- fase 2: commit
-- o si falla:
ROLLBACK PREPARED 'tx_pago_001';     -- fase 2: rollback

-- Ver transacciones preparadas pendientes
SELECT * FROM pg_prepared_xacts;
```

```rust
struct PreparedTransaction {
    gid:        String,        // ID global único
    txn_id:     TxnId,
    wal_lsn:    u64,           // posición en WAL — durabilidad garantizada
    prepared_at: Timestamp,
}

struct TwoPhaseCoordinator {
    prepared: DashMap<String, PreparedTransaction>,
}

impl TwoPhaseCoordinator {
    fn prepare(&self, txn: &Transaction, gid: &str) -> Result<()> {
        // Flushear WAL → durabilidad garantizada
        txn.wal.flush()?;
        self.prepared.insert(gid.to_string(), PreparedTransaction {
            gid: gid.to_string(), txn_id: txn.id, wal_lsn: txn.wal.lsn(), ..
        });
        Ok(())
    }

    fn commit_prepared(&self, gid: &str) -> Result<()> {
        let prep = self.prepared.remove(gid).ok_or(DbError::NoSuchTransaction)?;
        Engine::commit_txn(prep.txn_id)
    }
}
```

---

### DDL Triggers (Event Triggers)

```sql
-- Ejecutar lógica cuando se modifica el ESQUEMA (no los datos)
CREATE OR REPLACE FUNCTION log_ddl() RETURNS event_trigger AS $$
BEGIN
  INSERT INTO schema_audit_log (event, object_type, object_identity, happened_at)
  VALUES (TG_EVENT, TG_TAG, TG_ARGV[0], NOW());
END;
$$ LANGUAGE plpgsql;

CREATE EVENT TRIGGER audit_schema_changes
  ON ddl_command_end
  EXECUTE FUNCTION log_ddl();

-- Ahora cualquier ALTER TABLE, CREATE INDEX, DROP TABLE...
-- genera un registro automático en schema_audit_log

-- Otros eventos
ON ddl_command_start   -- antes de ejecutar DDL
ON ddl_command_end     -- después de ejecutar DDL
ON sql_drop            -- cuando se elimina un objeto
ON table_rewrite       -- cuando se reescribe una tabla (VACUUM FULL, ALTER TYPE)
```

---

### TABLESPACES — Almacenamiento Tiered

```sql
-- Definir ubicaciones de almacenamiento
CREATE TABLESPACE nvme  LOCATION '/mnt/nvme/dbyo';   -- SSD rápido
CREATE TABLESPACE sata  LOCATION '/mnt/sata/dbyo';   -- HDD normal
CREATE TABLESPACE cold  LOCATION '/mnt/cold/dbyo';   -- almacenamiento frío

-- Asignar objetos a tablespaces
CREATE TABLE logs (...) TABLESPACE cold;              -- datos fríos en HDD
CREATE TABLE users (...) TABLESPACE nvme;             -- tabla caliente en SSD
CREATE INDEX idx_email ON users(email) TABLESPACE nvme; -- índice en NVMe

-- Mover objetos entre tablespaces
ALTER TABLE logs SET TABLESPACE cold;

-- Ver distribución
SELECT tablename, tablespace FROM pg_tables ORDER BY tablespace;
```

---

### NOT VALID + VALIDATE CONSTRAINT — Constraints sin Downtime

```sql
-- Problema: ALTER TABLE ADD CONSTRAINT en tabla de millones de filas
-- bloquea la tabla y verifica TODOS los datos = downtime

-- Solución: agregar sin validar
ALTER TABLE orders
  ADD CONSTRAINT fk_user FOREIGN KEY (user_id) REFERENCES users(id)
  NOT VALID;                  -- instantáneo, no verifica existentes

-- Nueva constraint aplica solo a nuevos INSERTs/UPDATEs
-- Los datos viejos quedan sin validar

-- Validar después (en background, sin bloquear)
ALTER TABLE orders VALIDATE CONSTRAINT fk_user;
-- Lee la tabla y verifica — puede tomar tiempo pero no bloquea writes
```

---

### Configuración Dinámica (GUC — Grand Unified Configuration)

```sql
-- Ver configuración
SHOW max_connections;
SHOW statement_timeout;
SHOW ALL;                          -- todos los parámetros
SHOW search_path;

-- Cambiar en sesión actual
SET statement_timeout = '5s';
SET search_path = ventas, public;
SET work_mem = '64MB';            -- memoria para sorts y hashes
RESET statement_timeout;          -- volver al default

-- Cambiar para un usuario específico
ALTER USER ana SET statement_timeout = '10s';
ALTER USER bot SET work_mem = '16MB';

-- Cambiar globalmente (persiste al reiniciar)
ALTER SYSTEM SET max_connections = 200;
ALTER SYSTEM SET shared_buffers = '512MB';
SELECT dbyo_reload_config();      -- aplicar sin reiniciar

-- Ver de dónde viene cada valor
SELECT name, setting, source, context FROM pg_settings WHERE name = 'max_connections';
-- context: postmaster (requiere reinicio), sighup (reload), user (SET)
```

```rust
struct GucSystem {
    // Parámetros con su valor actual, default, y contexto de cambio
    params: HashMap<String, GucParam>,
}

struct GucParam {
    name:     String,
    value:    GucValue,
    default:  GucValue,
    context:  GucContext,   // Postmaster | Sighup | User | Transaction
    unit:     Option<String>, // "ms", "MB", "kB"...
    desc:     String,
}

enum GucValue { Bool(bool), Int(i64), Float(f64), Str(String) }
enum GucContext {
    Postmaster,   // requiere reiniciar el servidor
    Sighup,       // recargable con pg_reload_conf()
    User,         // cambiable con SET en cualquier momento
    Transaction,  // solo válido dentro de una transacción
}
```

---

### pg_stat_activity — Queries en Ejecución

```sql
-- Ver todas las queries activas
SELECT pid, usename, application_name, state,
       now() - query_start AS duration,
       wait_event_type, wait_event, query
FROM pg_stat_activity
WHERE state != 'idle'
ORDER BY duration DESC;

-- Ver locks actuales
SELECT pid, relation::regclass, mode, granted
FROM pg_locks
WHERE NOT granted;   -- locks esperando

-- Cancelar query específica (amigable)
SELECT pg_cancel_backend(pid) FROM pg_stat_activity
WHERE query LIKE '%tabla_grande%' AND duration > '30 seconds';

-- Terminar conexión (forzado)
SELECT pg_terminate_backend(pid) FROM pg_stat_activity
WHERE usename = 'usuario_problemático';
```

```rust
struct ActivityMonitor {
    connections: DashMap<ConnectionId, ConnectionInfo>,
}

struct ConnectionInfo {
    pid:              u32,
    user:             String,
    database:         String,
    state:            ConnState,     // Active | Idle | IdleInTx | FastPath
    query:            Option<String>,
    query_start:      Option<Instant>,
    wait_event:       Option<WaitEvent>,
    application_name: String,
}

enum WaitEvent {
    Lock(LockMode),
    IO(IoEvent),
    CPU,
    Client,   // esperando input del cliente
}

impl ActivityMonitor {
    fn cancel_backend(&self, pid: u32) -> bool {
        if let Some(conn) = self.connections.get(&pid) {
            conn.cancel_token.cancel();
            true
        } else { false }
    }
}
```

---

## Features Adicionales para Completitud Profesional

### Cifrado en Reposo

```sql
-- Cifrar BD completa con AES-256-GCM
CREATE DATABASE myapp
  WITH ENCRYPTION KEY 'vault://myapp/db-key';

-- Con contraseña derivada (PBKDF2 → AES-256)
CREATE DATABASE myapp
  WITH ENCRYPTION PASSWORD 'mi-clave-maestra';

-- Rotación de clave sin downtime
ALTER DATABASE myapp ROTATE ENCRYPTION KEY 'vault://myapp/db-key-v2';

-- Ver estado
SELECT datname, encrypted, key_version FROM pg_databases;
```

```rust
use aes_gcm::{Aes256Gcm, Key, Nonce, aead::{Aead, NewAead}};

struct EncryptedStorageEngine {
    inner:  StorageEngine,
    cipher: Aes256Gcm,
}

impl EncryptedStorageEngine {
    fn read_page(&self, page_id: u64) -> Result<[u8; 8192]> {
        let encrypted = self.inner.read_page_raw(page_id)?;
        let nonce = Nonce::from_slice(&encrypted[..12]);
        let plaintext = self.cipher.decrypt(nonce, &encrypted[12..])
            .map_err(|_| DbError::DecryptionFailed)?;
        Ok(plaintext.try_into().unwrap())
    }

    fn write_page(&self, page_id: u64, data: &[u8; 8192]) -> Result<()> {
        let nonce = generate_nonce(page_id);          // determinístico por page_id
        let encrypted = self.cipher.encrypt(&nonce, data.as_ref())?;
        self.inner.write_page_raw(page_id, &encrypted)
    }
}
```

Crate: `aes-gcm = "0.10"`, `pbkdf2 = "0.12"`.

---

### Data Masking — datos seguros para dev/test

```sql
-- Crear vista con datos enmascarados
CREATE MASKED VIEW users_dev AS
  SELECT
    id,
    MASK_EMAIL(email)          AS email,       -- 'ana@gmail.com' → 'a**@g*****.com'
    MASK_PHONE(telefono)       AS telefono,    -- '3001234567'   → '300****567'
    MASK_NAME(nombre)          AS nombre,      -- 'Ana García'   → 'A** G****'
    MASK_CC(credit_card)       AS credit_card, -- '4111...'      → '****-****-****-1111'
    PARTIAL(ssn, 'XXX-XX-', 4) AS ssn,        -- '123-45-6789'  → 'XXX-XX-6789'
    fecha_nacimiento                            -- no sensible
  FROM users;

-- Función de masking custom
CREATE MASKING FUNCTION mask_rut(val TEXT) RETURNS TEXT AS $$
  LEFT(val, 3) || '****' || RIGHT(val, 2)
$$;

-- Policy: aplicar masking automático por rol
CREATE MASKING POLICY politica_dev
  ON users FOR ROLE dev_team
  USING MASKED VIEW users_dev;
```

---

### Audit Trail Completo

```sql
-- Política de auditoría por tabla y operación
CREATE AUDIT POLICY datos_sensibles
  ON TABLE users
  FOR SELECT, UPDATE, DELETE
  COLUMNS (password_hash, email, credit_card)
  LOG TO TABLE security_audit_log;

CREATE AUDIT POLICY acceso_total
  ON DATABASE myapp
  FOR ALL
  WHEN user != 'internal_service'
  LOG TO TABLE audit_log;

-- Consultar
SELECT
  timestamp, usuario, ip_cliente, accion,
  tabla, columnas_accedidas,
  query_texto, rows_affected
FROM security_audit_log
WHERE timestamp > NOW() - INTERVAL '7 days'
ORDER BY timestamp DESC;
```

```rust
struct AuditLogger {
    policies: Vec<AuditPolicy>,
    writer:   Arc<Mutex<AuditWriter>>,
}

impl AuditLogger {
    fn record(&self, event: &QueryEvent) {
        for policy in &self.policies {
            if policy.matches(event) {
                self.writer.lock().write(AuditRecord {
                    timestamp:        Timestamp::now(),
                    user:             event.user.clone(),
                    client_ip:        event.client_ip,
                    action:           event.kind,
                    table:            event.table.clone(),
                    columns_accessed: event.columns.clone(),
                    query:            event.sql.clone(),
                    rows_affected:    event.rows_affected,
                });
            }
        }
    }
}
```

---

### PREPARE / EXECUTE — Prepared Statements

```sql
-- Compilar una vez: parseo + planificación = 0 overhead en ejecuciones siguientes
PREPARE buscar_user(INT) AS
  SELECT id, nombre, email FROM users WHERE id = $1;

PREPARE insertar_pedido(INT, DECIMAL, TEXT) AS
  INSERT INTO orders (user_id, total, estado) VALUES ($1, $2, $3)
  RETURNING id;

-- Ejecutar N veces sin re-parsear ni re-planificar
EXECUTE buscar_user(42);
EXECUTE buscar_user(99);
EXECUTE insertar_pedido(1, 150.00, 'pendiente');

-- Ver prepared statements de la sesión
SELECT name, data_type FROM pg_prepared_statements;

DEALLOCATE buscar_user;
DEALLOCATE ALL;
```

```rust
struct PreparedStatement {
    name:   String,
    plan:   Arc<PhysicalPlan>,    // plan compilado — reutilizable
    params: Vec<(String, DataType)>,
    sql:    String,
}

struct Session {
    prepared: HashMap<String, PreparedStatement>,
    // En el wire protocol: Extended Query Protocol
    // Parse → Bind → Execute (sin re-parsear)
}

impl Session {
    fn prepare(&mut self, name: &str, sql: &str, db: &Database) -> Result<()> {
        let ast   = db.parser().parse(sql)?;
        let plan  = db.planner().plan(&ast)?;
        let opt   = db.optimizer().optimize(plan)?;
        self.prepared.insert(name.to_string(), PreparedStatement {
            name: name.to_string(),
            plan: Arc::new(opt),
            params: extract_params(&ast),
            sql: sql.to_string(),
        });
        Ok(())
    }

    fn execute(&self, name: &str, params: Vec<Value>, db: &Database) -> Result<QueryResult> {
        let stmt = self.prepared.get(name).ok_or(DbError::NoSuchStatement)?;
        db.executor().execute_with_params(&stmt.plan, params)
    }
}
```

---

### Estadísticas Extendidas — Columnas Correlacionadas

```sql
-- Problema: el planner asume independencia entre columnas
-- ciudad='Bogotá' AND país='Colombia' → planner estima 50%×50%=25%
-- realidad: si ciudad=Bogotá entonces país=Colombia en 99% de casos

-- Solución: estadísticas multi-columna
CREATE STATISTICS stats_ciudad_pais (dependencies)
  ON ciudad, pais FROM clientes;

CREATE STATISTICS stats_dept_cargo (ndistinct, dependencies)
  ON departamento, cargo FROM empleados;

ANALYZE clientes;   -- actualizar estadísticas

-- El planner ahora estima correctamente
EXPLAIN ANALYZE SELECT * FROM clientes
WHERE ciudad = 'Bogotá' AND pais = 'Colombia';
-- Antes: rows=250 (25% de 1000)
-- Ahora: rows=980 (98% de 1000) — correcto
```

```rust
struct ExtendedStats {
    columns:      Vec<usize>,
    kind:         ExtendedStatsKind,
    dependencies: Option<FuncDependencies>,  // cuáles columnas determinan otras
    ndistinct:    Option<HashMap<Vec<usize>, f64>>, // distintos para combinaciones
}

enum ExtendedStatsKind { Dependencies, Ndistinct, MostCommon }

struct FuncDependencies {
    // P(col_b | col_a) — probabilidad condicional
    matrix: Vec<Vec<f64>>,
}
```

---

### FULL OUTER JOIN

```sql
-- Combina LEFT JOIN + RIGHT JOIN
SELECT u.nombre, o.total
FROM users u
FULL OUTER JOIN orders o ON u.id = o.user_id;
-- Incluye: usuarios sin pedidos (o.total = NULL)
--          pedidos sin usuario válido (u.nombre = NULL)
--          todos los matches normales

-- Útil para detectar inconsistencias de datos
SELECT
  u.id AS user_id_tabla,
  o.user_id AS user_id_pedido,
  u.nombre,
  o.total
FROM users u
FULL OUTER JOIN orders o ON u.id = o.user_id
WHERE u.id IS NULL OR o.user_id IS NULL;
-- Muestra datos huérfanos en ambas tablas
```

---

### Custom Aggregates y Operadores

```sql
-- Aggregate custom: MEDIAN
CREATE FUNCTION median_state(state FLOAT[], val FLOAT) RETURNS FLOAT[] AS $$
  array_append(state, val)
$$ LANGUAGE sql;

CREATE FUNCTION median_final(state FLOAT[]) RETURNS FLOAT AS $$
  state[array_length(state,1)/2 + 1]  -- elemento central ordenado
$$ LANGUAGE sql;

CREATE AGGREGATE MEDIAN(FLOAT) (
  SFUNC    = median_state,
  STYPE    = FLOAT[],
  FINALFUNC = median_final,
  INITCOND = '{}'
);

SELECT dept, MEDIAN(salary) FROM employees GROUP BY dept;

-- Operador custom para fuzzy equality
CREATE FUNCTION fuzzy_eq(a TEXT, b TEXT) RETURNS BOOL AS $$
  similarity(a, b) > 0.7
$$ LANGUAGE sql;

CREATE OPERATOR === (
  LEFTARG   = TEXT,
  RIGHTARG  = TEXT,
  PROCEDURE = fuzzy_eq,
  COMMUTATOR = ===
);

SELECT * FROM users WHERE nombre === 'jose';
```

---

### Geospatial Básico — Tipos y Funciones

```sql
-- Tipos geométricos
CREATE TABLE tiendas (
  id        INT PRIMARY KEY,
  nombre    TEXT,
  ubicacion POINT,     -- (longitud, latitud)
  zona      POLYGON    -- área de cobertura
);

INSERT INTO tiendas VALUES
  (1, 'Tienda Norte', POINT(-74.05, 4.75), NULL);

-- Distancia en km (Haversine)
SELECT nombre,
  ST_DISTANCE_KM(ubicacion, POINT(-74.08, 4.71)) AS dist_km
FROM tiendas
WHERE ST_DISTANCE_KM(ubicacion, POINT(-74.08, 4.71)) < 5
ORDER BY dist_km;

-- Contención
SELECT * FROM zonas WHERE ST_CONTAINS(area, POINT(-74.08, 4.71));

-- Índice R-Tree para consultas espaciales O(log n)
CREATE INDEX idx_geo ON tiendas USING rtree(ubicacion);
```

```rust
#[derive(Clone, Copy)]
struct Point { lon: f64, lat: f64 }

impl Point {
    // Fórmula Haversine — distancia en km sobre la esfera terrestre
    fn distance_km(&self, other: &Point) -> f64 {
        const R: f64 = 6371.0;
        let dlat = (other.lat - self.lat).to_radians();
        let dlon = (other.lon - self.lon).to_radians();
        let a = (dlat/2.0).sin().powi(2)
            + self.lat.to_radians().cos()
            * other.lat.to_radians().cos()
            * (dlon/2.0).sin().powi(2);
        R * 2.0 * a.sqrt().asin()
    }
}

// Índice R-Tree para búsqueda espacial
struct RTree { root: RTreeNode }

impl RTree {
    fn range_search(&self, center: Point, radius_km: f64) -> Vec<RecordId> {
        let bbox = BoundingBox::from_circle(center, radius_km);
        self.root.search(&bbox)
    }
}
```

Crate: `rstar = "0.12"` (R-Tree puro Rust).

---

### Query Result Cache

```sql
-- Caché automático para queries idénticas y frecuentes
SET query_cache_enabled = ON;
SET query_cache_size     = '256MB';
SET query_cache_ttl      = '60s';

-- Override por query
SELECT /*+ CACHE(ttl=300s) */ COUNT(*) FROM products WHERE active = true;
SELECT /*+ NO_CACHE */ SELECT * FROM orders WHERE id = 1;

-- Ver estado del cache
SELECT hits, misses, hit_rate, memory_used FROM query_cache_stats;

-- Invalidación automática cuando se modifica la tabla
UPDATE products SET stock = 0 WHERE id = 5;
-- → invalida automáticamente todas las queries cacheadas que leyeron products
```

```rust
struct QueryCache {
    entries:    DashMap<u64, CacheEntry>,   // fingerprint → resultado
    max_bytes:  usize,
    used_bytes: AtomicUsize,
    // Invalidación por tabla
    table_deps: DashMap<TableId, Vec<u64>>, // tabla → fingerprints que la leen
}

struct CacheEntry {
    result:    Arc<QueryResult>,
    expires_at: Instant,
    size_bytes: usize,
    hits:       AtomicU64,
}

impl QueryCache {
    fn get(&self, sql: &str) -> Option<Arc<QueryResult>> {
        let fp = fingerprint(sql);
        self.entries.get(&fp)
            .filter(|e| e.expires_at > Instant::now())
            .map(|e| { e.hits.fetch_add(1, Ordering::Relaxed); e.result.clone() })
    }

    fn invalidate_table(&self, table: TableId) {
        if let Some(fps) = self.table_deps.get(&table) {
            for fp in fps.iter() { self.entries.remove(fp); }
        }
    }
}
```

---

### Strict Mode — Sin Coerción Silenciosa

```sql
-- Comportamientos de MySQL que rechazamos:

-- ❌ MySQL: '1' = 1 = TRUE  (coerción implícita)
-- ✅ Nosotros: error de tipo

-- ❌ MySQL: INSERT 'hello world' en CHAR(5) → 'hello' (trunca silencioso)
-- ✅ Nosotros: ERROR: valor demasiado largo para CHAR(5)

-- ❌ MySQL: DATE '2026-02-30' → '2026-03-01' (fecha inválida convertida)
-- ✅ Nosotros: ERROR: fecha inválida '2026-02-30'

-- ❌ MySQL: SELECT col1, col2 GROUP BY col1 (col2 no está en GROUP BY)
-- ✅ Nosotros: ERROR: col2 debe estar en GROUP BY o en función de agregación

-- ❌ MySQL: división por cero → NULL (silencioso)
-- ✅ Nosotros: ERROR: división por cero

-- Configuración
SET strict_mode = ON;   -- default ON (recomendado)
SET strict_mode = OFF;  -- modo legacy/compatibilidad
```

```rust
struct StrictModeChecker;

impl StrictModeChecker {
    fn check_type_coercion(from: &DataType, to: &DataType) -> Result<()> {
        if !is_safe_implicit_cast(from, to) {
            return Err(DbError::TypeMismatch {
                expected: to.clone(),
                got:      from.clone(),
                hint: format!("usa CAST(valor AS {})", to),
            });
        }
        Ok(())
    }

    fn check_string_length(val: &str, max: usize) -> Result<()> {
        if val.chars().count() > max {
            return Err(DbError::StringTooLong { len: val.chars().count(), max });
        }
        Ok(())
    }

    fn check_group_by(select_cols: &[Expr], group_by: &[Expr], aggs: &[AggExpr]) -> Result<()> {
        for col in select_cols {
            if !is_in_group_by(col, group_by) && !is_in_aggregate(col, aggs) {
                return Err(DbError::ColumnNotInGroupBy { column: col.name() });
            }
        }
        Ok(())
    }
}
```

---

### Logical Replication + Replication Slots

```sql
-- En el primary: publicar tablas específicas
CREATE PUBLICATION ventas_pub
  FOR TABLE orders, order_items, products;

CREATE PUBLICATION errores_pub
  FOR TABLE logs WHERE (level = 'ERROR');

-- En la réplica: suscribirse
CREATE SUBSCRIPTION ventas_sub
  CONNECTION 'host=primary port=3306 user=repl password=xxx dbname=myapp'
  PUBLICATION ventas_pub;

-- Slot nombrado: WAL no se descarta hasta que el consumidor lo lea
-- Útil para CDC pipelines que pueden atrasarse
CREATE REPLICATION SLOT mi_cdc_slot LOGICAL;

-- Ver estado
SELECT slot_name, active, restart_lsn, confirmed_flush_lsn
FROM pg_replication_slots;

-- Leer cambios del slot (para CDC)
SELECT * FROM pg_logical_slot_get_changes('mi_cdc_slot', NULL, NULL);
```

```rust
struct LogicalReplicationSlot {
    name:        String,
    plugin:      String,          // decodificador: 'dbyo_output', 'wal2json'
    restart_lsn: AtomicU64,       // WAL se guarda desde aquí
    confirmed:   AtomicU64,       // hasta aquí el consumidor confirmó
}

struct Publication {
    name:   String,
    tables: Vec<(TableId, Option<Expr>)>,  // (tabla, WHERE opcional)
}

impl WalDecoder {
    fn decode_to_logical(&self, entry: &WalEntry, pub: &Publication) -> Option<LogicalChange> {
        if !pub.includes(&entry.table, entry.row()) { return None; }
        Some(match entry.kind {
            WalKind::Insert => LogicalChange::Insert { table: entry.table, row: entry.new_row() },
            WalKind::Update => LogicalChange::Update { table: entry.table, old: entry.old_row(), new: entry.new_row() },
            WalKind::Delete => LogicalChange::Delete { table: entry.table, key: entry.old_row() },
        })
    }
}
```

---

### mTLS — Autenticación por Certificado

```toml
# dbyo.toml
[tls]
enabled      = true
cert         = "/etc/dbyo/server.crt"
key          = "/etc/dbyo/server.key"
ca           = "/etc/dbyo/ca.crt"
verify_client = true   # mTLS: el cliente también presenta certificado

[auth]
# Equivalente de pg_hba.conf
[[auth.rules]]
host     = "192.168.0.0/24"
user     = "ana"
database = "all"
method   = "scram-sha-256"

[[auth.rules]]
host     = "10.0.0.0/8"
user     = "service_account"
database = "myapp"
method   = "cert"      # autenticación por certificado cliente

[[auth.rules]]
host     = "0.0.0.0/0"
user     = "all"
database = "all"
method   = "reject"    # rechazar todo lo demás
```

---

## Arquitectura de Código — Diseño Escalable

### Principio: Workspace Rust con Crates Especializados

```
Un crate por responsabilidad → compilación paralela → tests aislados → límites claros
```

```
dbyo/                           ← workspace root
├── Cargo.toml                  ← workspace manifest
├── crates/
│   ├── dbyo-core/              ← tipos base, errores, traits — SIN dependencias
│   ├── dbyo-types/             ← Value enum, DataType, collation, encoding
│   ├── dbyo-storage/           ← mmap, páginas, free list, TOAST
│   ├── dbyo-wal/               ← WAL writer/reader, crash recovery
│   ├── dbyo-index/             ← B+ Tree CoW, HNSW, GIN, GiST, BRIN, Hash, FTS
│   ├── dbyo-mvcc/              ← transacciones, snapshot isolation, SSI, locks
│   ├── dbyo-catalog/           ← schema, estadísticas, information_schema
│   ├── dbyo-sql/               ← parser (nom), AST, planner, optimizer, executor
│   ├── dbyo-functions/         ← todas las funciones built-in (string, math, date)
│   ├── dbyo-network/           ← MySQL wire protocol + PostgreSQL wire protocol
│   ├── dbyo-security/          ← RBAC, RLS, TLS, Argon2, audit, masking
│   ├── dbyo-replication/       ← streaming replication, logical replication, PITR
│   ├── dbyo-plugins/           ← WASM runtime, Lua scripting, UDFs, triggers
│   ├── dbyo-cache/             ← query result cache, buffer management
│   ├── dbyo-geo/               ← tipos geométricos, R-Tree, ST_* functions
│   ├── dbyo-vector/            ← VECTOR(n), HNSW, cuantización, similitud
│   ├── dbyo-migrations/        ← CLI de migrations, schema versioning
│   ├── dbyo-server/            ← binario: modo servidor (TCP daemon)
│   └── dbyo-embedded/          ← cdylib: modo embebido + C FFI
├── tests/                      ← integration tests (usan toda la pila)
├── benches/                    ← benchmarks con criterion
└── tools/
    ├── dbyo-cli/               ← cliente interactivo (como psql)
    └── dbyo-migrate/           ← herramienta de migración desde MySQL/PostgreSQL
```

### Dependencias entre crates (sin ciclos)

```
dbyo-core  ←──────────────────── base de todo (sin deps externas)
    ↑
dbyo-types ←──────────────────── Value, DataType, collation
    ↑
dbyo-storage  dbyo-wal           I/O físico
    ↑              ↑
dbyo-index ←──────┘              índices sobre storage + WAL
    ↑
dbyo-mvcc  ←──────────────────── transacciones sobre índices
    ↑
dbyo-catalog ─────────────────── schema + estadísticas
    ↑
dbyo-sql ─────────────────────── parser + planner + executor
    ↑              ↑
dbyo-functions    dbyo-plugins   funciones + extensiones
    ↑
dbyo-security ────────────────── RBAC, RLS, audit
    ↑
dbyo-replication ─────────────── WAL streaming + logical
    ↑
dbyo-network ─────────────────── MySQL + PostgreSQL wire protocol
    ↑
dbyo-server   dbyo-embedded      entry points
```

### Trait central: StorageEngine

```rust
// dbyo-core/src/traits.rs
// Todo el motor depende de este trait, no de implementaciones concretas

pub trait StorageEngine: Send + Sync {
    fn read_page(&self, page_id: u64)  -> Result<PageRef>;
    fn write_page(&self, page_id: u64, data: &Page) -> Result<()>;
    fn alloc_page(&self)               -> Result<u64>;
    fn free_page(&self, page_id: u64)  -> Result<()>;
    fn flush(&self)                    -> Result<()>;
}

// Implementaciones intercambiables sin cambiar el motor
pub struct MmapStorage   { .. }   // producción
pub struct MemoryStorage { .. }   // :memory: y tests
pub struct EncryptedStorage<S: StorageEngine> { inner: S, cipher: Aes256Gcm }
pub struct FaultInjector<S: StorageEngine>    { inner: S, rng: StdRng }  // tests
```

### Trait central: Index

```rust
pub trait Index: Send + Sync {
    fn insert(&self, key: &[u8], rid: RecordId, txn: &Transaction) -> Result<()>;
    fn delete(&self, key: &[u8], rid: RecordId, txn: &Transaction) -> Result<()>;
    fn lookup(&self, key: &[u8], txn: &Transaction)                 -> Result<Vec<RecordId>>;
    fn range(&self, lo: Bound<&[u8]>, hi: Bound<&[u8]>, txn: &Transaction)
        -> Result<Box<dyn Iterator<Item = RecordId>>>;
}

pub struct BTreeIndex  { .. }
pub struct HashIndex   { .. }
pub struct GinIndex    { .. }
pub struct BrinIndex   { .. }
pub struct HnswIndex   { .. }
pub struct FtsIndex    { .. }
```

### Engine central — punto de entrada único

```rust
// dbyo-core/src/engine.rs
pub struct Engine {
    // Storage
    storage:     Arc<dyn StorageEngine>,
    wal:         Arc<WalWriter>,

    // Catalog
    catalog:     Arc<RwLock<Catalog>>,
    stats:       Arc<StatsCatalog>,

    // Concurrencia
    mvcc:        Arc<MvccManager>,
    locks:       Arc<LockManager>,

    // Query
    parser:      Arc<SqlParser>,
    planner:     Arc<QueryPlanner>,
    optimizer:   Arc<QueryOptimizer>,
    executor:    Arc<Executor>,
    functions:   Arc<FunctionRegistry>,

    // Seguridad
    rbac:        Arc<RbacSystem>,
    audit:       Arc<AuditLogger>,
    tls:         Option<Arc<TlsAcceptor>>,

    // Cache
    query_cache: Arc<QueryCache>,

    // Replicación
    replication: Option<Arc<ReplicationManager>>,

    // Plugins
    wasm:        Arc<WasmRuntime>,
    lua:         Arc<LuaRuntime>,

    // Observabilidad
    activity:    Arc<ActivityMonitor>,
    stat_stmts:  Arc<StatCollector>,
}

impl Engine {
    // Único punto de entrada para cualquier SQL
    pub async fn query(&self, sql: &str, session: &Session) -> Result<QueryResult> {
        // 1. Check query cache
        if let Some(cached) = self.query_cache.get(sql) { return Ok(cached); }

        // 2. Parse
        let ast = self.parser.parse(sql)?;

        // 3. Verificar permisos (RBAC + RLS)
        self.rbac.check(&ast, &session.user)?;

        // 4. Plan + Optimize
        let plan = self.planner.plan(&ast, &self.catalog)?;
        let opt  = self.optimizer.optimize(plan, &self.stats)?;

        // 5. Execute
        let txn    = self.mvcc.begin(session.isolation_level)?;
        let result = self.executor.execute(&opt, &txn, session).await?;
        txn.commit()?;

        // 6. Audit + Stats
        self.audit.record(&QueryEvent::from(&result, session));
        self.stat_stmts.record(sql, result.duration, result.rows);

        // 7. Cache si aplica
        if result.is_cacheable() { self.query_cache.put(sql, result.clone()); }

        Ok(result)
    }
}
```

### Patrón Event-Driven — WAL como bus de eventos

```rust
// Todo fluye del WAL — replicación, CDC, cache invalidation, triggers
struct WalEventBus {
    subscribers: Vec<Box<dyn WalSubscriber>>,
}

trait WalSubscriber: Send + Sync {
    fn on_commit(&self, entry: &WalEntry);
}

// Cada sistema se suscribe al WAL
impl WalSubscriber for ReplicationManager { .. }   // enviar a réplicas
impl WalSubscriber for CdcPublisher       { .. }   // change streams
impl WalSubscriber for QueryCache         { .. }   // invalidar cache
impl WalSubscriber for TriggerEngine      { .. }   // disparar triggers AFTER
impl WalSubscriber for AuditLogger        { .. }   // registrar cambios
```

### Estructura de un crate típico

```
crates/dbyo-storage/
├── Cargo.toml
├── src/
│   ├── lib.rs          ← re-exports públicos
│   ├── page.rs         ← formato de página
│   ├── mmap.rs         ← MmapStorage
│   ├── memory.rs       ← MemoryStorage
│   ├── encrypted.rs    ← EncryptedStorage<S>
│   ├── free_list.rs    ← páginas libres
│   └── toast.rs        ← overflow de valores grandes
└── tests/
    ├── page_test.rs
    ├── mmap_test.rs
    └── fault_injection_test.rs
```

### Cargo.toml del workspace

```toml
# dbyo/Cargo.toml
[workspace]
resolver = "2"
members = [
    "crates/dbyo-core",
    "crates/dbyo-types",
    "crates/dbyo-storage",
    "crates/dbyo-wal",
    "crates/dbyo-index",
    "crates/dbyo-mvcc",
    "crates/dbyo-catalog",
    "crates/dbyo-sql",
    "crates/dbyo-functions",
    "crates/dbyo-network",
    "crates/dbyo-security",
    "crates/dbyo-replication",
    "crates/dbyo-plugins",
    "crates/dbyo-cache",
    "crates/dbyo-geo",
    "crates/dbyo-vector",
    "crates/dbyo-migrations",
    "crates/dbyo-server",
    "crates/dbyo-embedded",
    "tools/dbyo-cli",
    "tools/dbyo-migrate",
]

[workspace.dependencies]
# Compartidas entre todos los crates (versión única en todo el workspace)
tokio        = { version = "1",    features = ["full"] }
serde        = { version = "1",    features = ["derive"] }
thiserror    = "1"
anyhow       = "1"
tracing      = "0.1"
dashmap      = "5"
arc-swap     = "1"

[profile.release]
opt-level   = 3
lto         = "fat"        # link-time optimization
codegen-units = 1          # máxima optimización (compila más lento)
panic       = "abort"      # sin unwinding = binario más pequeño

[profile.bench]
inherits = "release"
debug    = true            # símbolos para profiling
```

---

## Preparación para IA — AI-Native Database

### Vector Search Híbrido — BM25 + Semántico (RRF)

El estándar actual en RAG. Combina relevancia léxica y semántica en un solo ranking.

```sql
-- Búsqueda híbrida con Reciprocal Rank Fusion
SELECT id, titulo,
  FTS_RANK(contenido, 'amor eterno')                       AS score_bm25,
  1 - (embedding <=> AI_EMBED('amor eterno'))              AS score_vector,
  RRF(
    FTS_RANK(contenido, 'amor eterno'),
    embedding <=> AI_EMBED('amor eterno')
  )                                                        AS score_final
FROM documentos
ORDER BY score_final DESC
LIMIT 10;

-- Con re-ranking cross-encoder (más preciso, más lento)
SELECT * FROM HYBRID_SEARCH(
  query      => '¿Qué dice sobre el perdón?',
  table      => 'versiculos',
  fts_col    => 'texto',
  vec_col    => 'embedding',
  top_k      => 20,        -- recuperar 20 candidatos
  rerank_k   => 5,         -- re-rankear y retornar 5
  reranker   => 'cross-encoder'
);
```

```rust
// Reciprocal Rank Fusion — estándar para combinar rankings
fn rrf(ranks: &[usize], k: f64) -> f64 {
    ranks.iter().map(|&r| 1.0 / (k + r as f64)).sum()
}

struct HybridSearcher {
    fts:    Arc<FtsIndex>,
    vector: Arc<HnswIndex>,
    reranker: Option<Arc<dyn CrossEncoder>>,
}

impl HybridSearcher {
    async fn search(&self, query: &str, top_k: usize) -> Vec<(RecordId, f64)> {
        // Ejecutar ambas búsquedas en paralelo
        let (bm25_results, vec_results) = tokio::join!(
            self.fts.search(query, top_k * 2),
            self.vector.search(&self.embed(query).await, top_k * 2),
        );

        // Calcular RRF scores para cada documento
        let mut scores: HashMap<RecordId, f64> = HashMap::new();
        for (rank, (id, _)) in bm25_results.iter().enumerate() {
            *scores.entry(*id).or_default() += rrf(&[rank], 60.0);
        }
        for (rank, (id, _)) in vec_results.iter().enumerate() {
            *scores.entry(*id).or_default() += rrf(&[rank], 60.0);
        }

        // Ordenar por RRF score y re-rankear si hay cross-encoder
        let mut ranked: Vec<_> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        ranked.truncate(top_k);

        if let Some(reranker) = &self.reranker {
            reranker.rerank(query, &mut ranked).await;
        }
        ranked
    }
}
```

---

### Funciones AI Built-in

```sql
-- Configurar backend de IA
-- dbyo.toml
-- [ai]
-- provider = "ollama"                    # local, sin costo, sin privacidad de datos
-- embed_model = "nomic-embed-text"       # embeddings
-- llm_model   = "llama3.1"              # generación de texto
-- endpoint    = "http://localhost:11434"
--
-- [ai.fallback]                          # si Ollama no está disponible
-- provider = "openai"
-- api_key  = "${OPENAI_API_KEY}"

-- Generar embeddings
SELECT AI_EMBED('texto a embeber') AS vec;             -- VECTOR(768 o 1536)
SELECT AI_EMBED(contenido)         AS embedding FROM docs; -- embeber columna

-- Embedding automático al insertar
CREATE TABLE docs (
  id        INT PRIMARY KEY,
  contenido TEXT,
  embedding VECTOR(768) GENERATED ALWAYS AS (AI_EMBED(contenido)) STORED
);

-- Clasificación
SELECT AI_CLASSIFY(review,
  categories => ARRAY['positivo', 'negativo', 'neutro']
) AS sentimiento FROM reviews;

-- Extracción estructurada (JSON)
SELECT AI_EXTRACT(descripcion,
  schema => '{"nombre":"string","precio":"number","categoria":"string"}'
) AS datos FROM productos_raw;

-- Resumen
SELECT AI_SUMMARIZE(contenido, max_words => 100) AS resumen
FROM articulos WHERE LENGTH(contenido) > 2000;

-- Traducción
SELECT AI_TRANSLATE(texto, target => 'en') AS english FROM contenidos;

-- Detección y masking de PII
SELECT AI_DETECT_PII(comentario)  AS tiene_datos_personales,
       AI_MASK_PII(comentario)    AS comentario_seguro
FROM feedback;

-- Completar texto / SQL
SELECT AI_COMPLETE('El usuario preguntó sobre ') AS sugerencia;
```

```rust
#[async_trait]
trait AiBackend: Send + Sync {
    async fn embed(&self, text: &str)                              -> Result<Vec<f32>>;
    async fn classify(&self, text: &str, labels: &[&str])         -> Result<String>;
    async fn extract(&self, text: &str, schema: &str)             -> Result<Value>;
    async fn summarize(&self, text: &str, max_words: usize)       -> Result<String>;
    async fn translate(&self, text: &str, target_lang: &str)      -> Result<String>;
    async fn complete(&self, prompt: &str)                         -> Result<String>;
}

struct OllamaBackend   { client: reqwest::Client, endpoint: String, model: String }
struct OpenAiBackend   { client: async_openai::Client }
struct LocalBackend    { model: Arc<dyn EmbeddingModel + Send + Sync> }

// Router con fallback automático
struct AiRouter {
    primary:  Box<dyn AiBackend>,
    fallback: Option<Box<dyn AiBackend>>,
    cache:    Arc<LruCache<String, Vec<f32>>>,  // cache de embeddings
}

impl AiRouter {
    async fn embed(&self, text: &str) -> Result<Vec<f32>> {
        // Cache: mismo texto = mismo embedding
        let key = format!("embed:{}", text);
        if let Some(cached) = self.cache.get(&key) { return Ok(cached.clone()); }

        let result = match self.primary.embed(text).await {
            Ok(v) => v,
            Err(_) => self.fallback.as_ref()
                .ok_or(DbError::AiUnavailable)?
                .embed(text).await?,
        };
        self.cache.put(key, result.clone());
        Ok(result)
    }
}
```

Crates: `ollama-rs = "0.2"`, `async-openai = "0.23"`.

---

### RAG Pipeline Nativo

```sql
CREATE RAG PIPELINE biblia_qa (
  source_table  = versiculos,
  content_col   = texto,
  embedding_col = embedding,
  llm_provider  = 'ollama',
  llm_model     = 'llama3.1',
  top_k         = 5,
  search_mode   = 'hybrid',
  rerank        = true,
  system_prompt = 'Eres un experto en teología bíblica. Responde basándote solo en los versículos proporcionados.'
);

-- Usar el pipeline
SELECT respuesta, fuentes, tokens_usados
FROM RAG_QUERY(
  pipeline => 'biblia_qa',
  question => '¿Qué dice la Biblia sobre el perdón de los pecados?'
);

-- Resultado:
-- respuesta: "Según 1 Juan 1:9, si confesamos nuestros pecados, Dios es fiel y justo..."
-- fuentes:   [{"libro":"1Juan","capitulo":1,"versiculo":9,"score":0.97}, ...]
-- tokens:    412
```

```rust
struct RagPipeline {
    searcher:    HybridSearcher,
    llm:         Arc<dyn AiBackend>,
    system_prompt: String,
    top_k:       usize,
}

impl RagPipeline {
    async fn query(&self, question: &str, db: &Database) -> Result<RagResult> {
        // 1. Recuperar documentos relevantes
        let docs = self.searcher.search(question, self.top_k).await?;
        let context = docs.iter()
            .map(|(id, score)| db.fetch_content(*id))
            .collect::<Vec<_>>().join("\n\n---\n\n");

        // 2. Construir prompt con contexto
        let prompt = format!(
            "Contexto:\n{context}\n\nPregunta: {question}\nRespuesta:"
        );

        // 3. Generar respuesta con LLM
        let answer = self.llm.complete(&prompt).await?;

        Ok(RagResult { answer, sources: docs, tokens: estimate_tokens(&prompt) })
    }
}
```

---

### Feature Store

```sql
CREATE FEATURE GROUP user_features (
  entity_id INT,
  timestamp TIMESTAMPTZ,

  total_pedidos      INT     AS (SELECT COUNT(*) FROM orders
                                 WHERE user_id = entity_id),
  monto_30d          DECIMAL AS (SELECT COALESCE(SUM(total), 0)
                                 FROM orders WHERE user_id = entity_id
                                 AND created_at > NOW() - INTERVAL '30 days'),
  dias_ultimo_pedido INT     AS (SELECT COALESCE(
                                   DATE_PART('day', NOW() - MAX(created_at)), 999)
                                 FROM orders WHERE user_id = entity_id),
  categoria_favorita TEXT    AS (SELECT p.categoria
                                 FROM order_items oi
                                 JOIN products p ON oi.product_id = p.id
                                 JOIN orders o ON oi.order_id = o.id
                                 WHERE o.user_id = entity_id
                                 GROUP BY p.categoria
                                 ORDER BY COUNT(*) DESC LIMIT 1)
)
REFRESH EVERY '1 hour'
POINT_IN_TIME CORRECT;

-- Online serving (tiempo real, producción)
SELECT * FROM GET_FEATURES('user_features', entity_id => 42);

-- Historical serving (entrenamiento — point-in-time correcto)
SELECT f.*
FROM GET_FEATURES_HISTORY(
  feature_group => 'user_features',
  entity_ids    => (SELECT id FROM users WHERE created_at > '2025-01-01'),
  as_of         => '2025-12-31 23:59:59'   -- estado exacto en esa fecha
) AS f;
```

```rust
struct FeatureStore {
    groups:  HashMap<String, FeatureGroup>,
    storage: Arc<StorageEngine>,
    cache:   Arc<QueryCache>,
}

struct FeatureGroup {
    name:       String,
    features:   Vec<FeatureDef>,
    refresh_ms: u64,
    snapshots:  BTreeMap<i64, Snapshot>,   // timestamp → snapshot (point-in-time)
}

impl FeatureStore {
    // Entrenamiento: valores exactos en un momento histórico
    fn get_historical(&self, group: &str, entity_id: i64, as_of: i64) -> Result<Row> {
        let snap = self.groups[group].snapshots
            .range(..=as_of).next_back()
            .ok_or(DbError::NoSnapshotAvailable)?;
        snap.1.get(entity_id)
    }

    // Producción: valores actuales con cache
    async fn get_online(&self, group: &str, entity_id: i64) -> Result<Row> {
        let key = format!("feat:{}:{}", group, entity_id);
        if let Some(cached) = self.cache.get(&key) { return Ok(cached); }
        let val = self.compute_features(group, entity_id).await?;
        self.cache.put_ttl(&key, val.clone(), Duration::from_secs(60));
        Ok(val)
    }
}
```

---

### Model Store — Modelos ONNX en la BD

```sql
-- Registrar modelo
CREATE MODEL spam_classifier (
  framework   = 'onnx',
  file        = '/models/spam_v2.onnx',
  version     = 2,
  input_cols  = ARRAY['asunto TEXT', 'cuerpo TEXT'],
  output_col  = 'prob_spam FLOAT',
  description = 'Clasificador de spam para emails v2'
);

-- Predicción en SQL
SELECT asunto,
  PREDICT(spam_classifier, asunto, cuerpo) AS prob_spam
FROM emails
WHERE PREDICT(spam_classifier, asunto, cuerpo) > 0.85
ORDER BY prob_spam DESC;

-- A/B testing de versiones
ALTER MODEL spam_classifier ADD VERSION 3 FROM '/models/spam_v3.onnx';

SELECT PREDICT_AB(
  model      => 'spam_classifier',
  versions   => ARRAY[2, 3],
  traffic    => ARRAY[0.5, 0.5],
  entity_id  => email_id
) AS prob_spam FROM emails;

-- Métricas de uso
SELECT version, calls, avg_latency_ms, p99_latency_ms
FROM model_stats WHERE model = 'spam_classifier';
```

```rust
use ort::{Environment, Session, inputs};

struct ModelStore {
    models:  DashMap<String, Vec<OnnxModel>>,   // nombre → versiones
    env:     Arc<Environment>,
}

struct OnnxModel {
    version:  u32,
    session:  Session,
    active:   AtomicBool,
    calls:    AtomicU64,
    total_ms: AtomicU64,
}

impl ModelStore {
    fn predict(&self, name: &str, inputs: &[Value]) -> Result<Vec<f32>> {
        let model = self.models.get(name)
            .and_then(|versions| versions.iter().find(|m| m.active.load(Ordering::Relaxed)))
            .ok_or(DbError::ModelNotFound)?;

        let start = Instant::now();
        let outputs = model.session.run(inputs)?;
        model.calls.fetch_add(1, Ordering::Relaxed);
        model.total_ms.fetch_add(start.elapsed().as_millis() as u64, Ordering::Relaxed);

        Ok(outputs[0].try_extract_tensor::<f32>()?.view().to_owned().into_raw_vec())
    }
}
```

Crate: `ort = "2"` (ONNX Runtime — corre modelos de PyTorch, TensorFlow, scikit-learn).

---

### Índices Adaptativos — Aprenden del Uso

```sql
-- Activar modo adaptativo
SET adaptive_indexing = ON;
SET index_suggestion_min_queries = 100;
SET index_auto_create = false;   -- sugerir pero no crear sin aprobación

-- Ver sugerencias generadas por el motor
SELECT tabla, columnas, tipo_indice,
       speedup_estimado, queries_por_dia, espacio_mb
FROM db_index_suggestions
ORDER BY speedup_estimado DESC;

-- tabla     | columnas           | tipo  | speedup | queries/día | mb
-- orders    | (user_id, status)  | btree | 45x     | 1247        | 12
-- products  | (categoria, precio)| btree | 12x     | 893         | 8

-- Aplicar sugerencia (CREATE INDEX CONCURRENTLY)
SELECT apply_index_suggestion(suggestion_id => 1);

-- Ver índices sin uso (candidatos a eliminar)
SELECT indexname, tablename, size_pretty, last_used
FROM db_unused_indexes
WHERE last_used < NOW() - INTERVAL '30 days'
OR last_used IS NULL
ORDER BY size_bytes DESC;

-- Eliminar índices inútiles
SELECT drop_unused_index(indexname => 'idx_orders_old_status');
```

```rust
struct AdaptiveIndexManager {
    query_log: Arc<StatCollector>,
    catalog:   Arc<RwLock<Catalog>>,
    threshold: u64,
}

impl AdaptiveIndexManager {
    fn analyze(&self) -> Vec<IndexSuggestion> {
        // Extraer patrones de filtro de queries frecuentes
        self.query_log.top_patterns(1000).into_iter()
            .filter(|p| p.frequency >= self.threshold)
            .filter(|p| !self.catalog.read().has_index_for(p))
            .map(|p| IndexSuggestion {
                table:     p.table.clone(),
                columns:   p.filter_cols.clone(),
                idx_type:  p.best_index_type(),
                speedup:   p.estimate_speedup(),
                queries:   p.frequency,
                size_mb:   p.estimate_size_mb(&self.catalog.read()),
            })
            .collect()
    }

    fn unused_indexes(&self) -> Vec<UnusedIndex> {
        self.catalog.read().all_indexes().into_iter()
            .filter(|idx| self.query_log.index_usage(idx.name()) == 0)
            .map(|idx| UnusedIndex { name: idx.name(), size_bytes: idx.size() })
            .collect()
    }
}
```

---

### Text-to-SQL — Consultas en Lenguaje Natural

```sql
-- Preguntar en español, obtener SQL + resultado
SELECT NL_QUERY('¿Cuántos usuarios se registraron el mes pasado?');

SELECT NL_QUERY('muéstrame los 5 productos más vendidos este trimestre');

SELECT NL_QUERY('¿cuál es el promedio de ventas por categoría?');

-- Solo ver el SQL generado sin ejecutar
SELECT NL_TO_SQL('¿cuántas órdenes hay pendientes por usuario?');
-- → SELECT u.nombre, COUNT(*) AS pedidos_pendientes
--   FROM users u JOIN orders o ON u.id = o.user_id
--   WHERE o.status = 'pendiente'
--   GROUP BY u.nombre ORDER BY pedidos_pendientes DESC

-- Explicar resultado en lenguaje natural
SELECT NL_EXPLAIN(
  query  => 'SELECT AVG(total) FROM orders WHERE MONTH(created_at) = 3',
  locale => 'es'
);
-- → "El promedio de ventas en marzo fue $1,247.50, sobre 3,842 pedidos."

-- Autocompletar SQL mientras escribes
SELECT NL_AUTOCOMPLETE('SELECT u.nombre FROM users u WHERE u.'); -- sugiere columnas
```

```rust
struct NlSqlEngine {
    llm:    Arc<dyn AiBackend>,
    schema: Arc<SchemaContext>,   // schema resumido para el prompt
}

impl NlSqlEngine {
    async fn to_sql(&self, question: &str, db_name: &str) -> Result<String> {
        let schema_summary = self.schema.summarize(db_name);

        let prompt = format!(r#"
Eres un experto en SQL. Convierte la pregunta en SQL válido para esta base de datos.

Esquema:
{schema_summary}

Pregunta: {question}

Reglas:
- Solo genera SELECT (nunca INSERT/UPDATE/DELETE)
- Usa aliases claros
- Limita a 1000 filas si no se especifica
- Responde SOLO con el SQL, sin explicación

SQL:
"#);

        let sql = self.llm.complete(&prompt).await?;
        // Validar que el SQL es seguro antes de ejecutar
        self.validate_readonly_sql(&sql)?;
        Ok(sql.trim().to_string())
    }
}
```

---

### Detección de Anomalías

```sql
-- Crear detector
CREATE ANOMALY DETECTOR ventas_anomalas ON orders (
  metric      = total,
  group_by    = ARRAY[user_id],
  window      = '30 days',
  algorithm   = 'isolation_forest',   -- isolation_forest | zscore | mad
  sensitivity = 'medium',
  alert_table = 'anomaly_alerts'
);

-- Score de anomalía por fila
SELECT user_id, total, created_at,
  ANOMALY_SCORE(
    total,
    method => 'zscore',
    window => (PARTITION BY user_id ORDER BY created_at ROWS 30 PRECEDING)
  ) AS score
FROM orders
WHERE score > 2.5   -- más de 2.5 desviaciones estándar
ORDER BY score DESC;

-- Ver alertas
SELECT detector, entidad, valor, score, detected_at, descripcion
FROM anomaly_alerts
WHERE detected_at > NOW() - INTERVAL '24 hours'
ORDER BY score DESC;
```

```rust
enum AnomalyAlgorithm { ZScore, Mad, IsolationForest, LocalOutlierFactor }

struct AnomalyDetector {
    algorithm:  AnomalyAlgorithm,
    window:     Duration,
    threshold:  f64,
}

impl AnomalyDetector {
    fn score_zscore(&self, value: f64, history: &[f64]) -> f64 {
        let mean  = history.iter().sum::<f64>() / history.len() as f64;
        let var   = history.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / history.len() as f64;
        let stdev = var.sqrt();
        if stdev == 0.0 { 0.0 } else { ((value - mean) / stdev).abs() }
    }

    fn score_mad(&self, value: f64, history: &[f64]) -> f64 {
        // Median Absolute Deviation — más robusto que Z-score ante outliers
        let mut sorted = history.to_vec();
        sorted.sort_by(f64::total_cmp);
        let median = sorted[sorted.len() / 2];
        let mut deviations: Vec<f64> = history.iter().map(|x| (x - median).abs()).collect();
        deviations.sort_by(f64::total_cmp);
        let mad = deviations[deviations.len() / 2];
        if mad == 0.0 { 0.0 } else { (value - median).abs() / mad }
    }
}
```

---

### Privacidad Diferencial

```sql
-- Consultas estadísticas con privacidad matemáticamente garantizada
-- Epsilon: menor = más privado, mayor = más preciso
-- (0.1 = muy privado, 10.0 = menos privado)

SELECT DP_COUNT(*)    FROM users          WITH PRIVACY (epsilon => 1.0);
SELECT DP_AVG(salary) FROM employees      WITH PRIVACY (epsilon => 0.5);
SELECT DP_SUM(total)  FROM orders         WITH PRIVACY (epsilon => 0.1, delta => 1e-5);
SELECT DP_HISTOGRAM(categoria) FROM products WITH PRIVACY (epsilon => 1.0);

-- El resultado es estadísticamente útil pero ningún individuo puede ser
-- identificado con certeza matemáticamente demostrable

-- Presupuesto de privacidad por usuario/rol
GRANT DP_SELECT ON users TO investigador
  WITH PRIVACY BUDGET (epsilon_total => 10.0, reset_after => '30 days');

-- Ver presupuesto restante
SELECT epsilon_usado, epsilon_total, epsilon_restante
FROM privacy_budgets WHERE role = 'investigador';
```

```rust
use opendp::prelude::*;

struct DifferentialPrivacy;

impl DifferentialPrivacy {
    fn dp_count(true_count: u64, epsilon: f64) -> i64 {
        // Mecanismo Laplace: ruido calibrado a sensibilidad/epsilon
        let sensitivity = 1.0;   // COUNT tiene sensibilidad 1
        let scale       = sensitivity / epsilon;
        let noise       = laplace_noise(scale);
        (true_count as f64 + noise).round() as i64
    }

    fn dp_sum(true_sum: f64, epsilon: f64, sensitivity: f64) -> f64 {
        // sensitivity = max_value - min_value del dominio
        let scale = sensitivity / epsilon;
        true_sum + laplace_noise(scale)
    }

    fn dp_avg(values: &[f64], epsilon: f64, min: f64, max: f64) -> f64 {
        // Dividir epsilon entre count y sum para composición
        let eps_each   = epsilon / 2.0;
        let dp_n       = Self::dp_count(values.len() as u64, eps_each) as f64;
        let dp_s       = Self::dp_sum(values.iter().sum(), eps_each, max - min);
        dp_s / dp_n.max(1.0)
    }
}

fn laplace_noise(scale: f64) -> f64 {
    let u: f64 = rand::random::<f64>() - 0.5;
    -scale * u.signum() * (1.0 - 2.0 * u.abs()).ln()
}
```

Crate: `opendp = "0.10"`.

---

### Data Lineage — Trazabilidad para IA y GDPR

```sql
-- ¿De dónde vino esta fila?
SELECT * FROM DATA_LINEAGE('orders', row_id => 42);
-- source:    'CSV import 2026-01-15, archivo: ventas.csv, fila: 1847'
-- modified:  'UPDATE 2026-02-10 por ana@empresa.com'
-- derived:   'included in materialized view: ventas_mensuales'
-- training:  'included in ML dataset: churn_model_v2 (2025-12-01)'

-- GDPR: ¿dónde están los datos de este usuario?
SELECT * FROM DATA_LOCATIONS(user_id => 42);
-- tabla: users         → fila 42
-- tabla: orders        → 15 filas
-- tabla: audit_log     → 234 entradas
-- backup: 2026-03-01   → incluido
-- ml_model: churn_v2   → features incluidas en entrenamiento ← PROBLEMA GDPR

-- Marcar dataset de entrenamiento
CREATE TRAINING DATASET churn_v3 AS (
  SELECT user_id, total_pedidos, monto_30d, churn
  FROM user_features
  WHERE fecha_snapshot = '2026-01-01'
);
-- Registra automáticamente qué user_ids y qué snapshot se usaron

-- Ejecutar derecho al olvido (GDPR Art. 17)
SELECT EXECUTE_RIGHT_TO_BE_FORGOTTEN(user_id => 42, dry_run => true);
-- dry_run=true: muestra qué se borraría sin borrar
-- Resultado: lista todas las ubicaciones + advertencia sobre modelos ML
```

```rust
struct LineageTracker {
    graph: DashMap<(TableId, RowId), LineageNode>,
}

struct LineageNode {
    source:    DataSource,
    created:   Timestamp,
    modified:  Vec<Modification>,
    derived_in: Vec<DerivedUsage>,   // vistas, backups, snapshots
    ml_usage:  Vec<MlUsage>,         // qué modelos usaron esta fila
}

enum DataSource {
    CsvImport { file: String, row: u64, imported_at: Timestamp },
    ApiInsert { endpoint: String, user: String },
    Migration { from_db: String, from_table: String },
    Derived   { view: String, base_tables: Vec<String> },
}

impl LineageTracker {
    fn right_to_be_forgotten(&self, user_id: i64, dry_run: bool) -> ForgottenReport {
        // Buscar todas las tablas con FK a users.id = user_id
        // Buscar en backups, replicas, vistas materializadas
        // Alertar si hay modelos ML entrenados con datos del usuario
        ForgottenReport { locations: self.find_all(user_id), ml_risk: self.check_ml(user_id) }
    }
}
```

---

## Infraestructura Avanzada — Completitud Final

### Sharding — Distribución Horizontal

Cuando una sola máquina no da abasto. Partir los datos entre N nodos.

```sql
-- Shard por hash del campo distribuidor
CREATE TABLE orders (
  id      BIGINT PRIMARY KEY,
  user_id INT NOT NULL,
  total   DECIMAL
) DISTRIBUTED BY HASH(user_id) INTO 4 SHARDS;

-- El cliente ve una sola BD — el motor enruta internamente
SELECT * FROM orders WHERE user_id = 42;
-- → router calcula: 42 % 4 = 2 → va directo al nodo 2

-- Shard por rango (útil para datos de tiempo)
CREATE TABLE logs (ts TIMESTAMPTZ, msg TEXT)
DISTRIBUTED BY RANGE(ts) (
  SHARD s1 VALUES LESS THAN ('2025-01-01') ON NODE 'node1:3306',
  SHARD s2 VALUES LESS THAN ('2026-01-01') ON NODE 'node2:3306',
  SHARD s3 VALUES LESS THAN (MAXVALUE)     ON NODE 'node3:3306'
);

-- Cross-shard query (el coordinator hace merge)
SELECT user_id, SUM(total) FROM orders GROUP BY user_id;
-- → ejecuta en todos los shards en paralelo → merge en coordinator

-- Rebalancear shards (sin downtime)
ALTER TABLE orders REBALANCE SHARDS;
```

```rust
struct ShardRouter {
    shards:   Vec<ShardNode>,
    strategy: ShardStrategy,
}

enum ShardStrategy {
    Hash   { column: usize, buckets: u32 },
    Range  { column: usize, ranges: Vec<(Value, NodeId)> },
    List   { column: usize, values: HashMap<Value, NodeId> },
    RoundRobin,
}

impl ShardRouter {
    fn route(&self, row: &Row) -> NodeId {
        match &self.strategy {
            ShardStrategy::Hash { column, buckets } => {
                let hash = row.get(*column).hash();
                NodeId(hash % *buckets as u64)
            }
            ShardStrategy::Range { column, ranges } => {
                let val = row.get(*column);
                ranges.iter()
                    .find(|(upper, _)| &val < upper)
                    .map(|(_, node)| *node)
                    .unwrap_or(NodeId(0))
            }
            _ => todo!()
        }
    }

    async fn scatter_gather(&self, plan: &PhysicalPlan) -> Result<QueryResult> {
        // Enviar plan a todos los shards en paralelo
        let futures: Vec<_> = self.shards.iter()
            .map(|shard| shard.execute(plan))
            .collect();
        let results = futures::future::join_all(futures).await;

        // Merge de resultados (sort-merge o hash-merge)
        QueryResult::merge(results.into_iter().filter_map(|r| r.ok()).collect())
    }
}
```

---

### Hot Standby — Leer desde Réplicas

```sql
-- En la réplica: acepta reads mientras aplica WAL del primary
SET hot_standby = ON;
SET max_standby_streaming_delay = '30s';  -- esperar hasta 30s si hay conflicto WAL

-- Query readonly en la réplica (lag máximo configurable)
SET max_replication_lag_ms = 1000;  -- error si la réplica va >1s atrasada
SELECT COUNT(*) FROM orders;         -- OK si lag < 1s

-- En el balanceador: routing automático
SET load_balance_reads = ON;
SET replica_selection = 'least_lag'; -- elegir réplica con menor lag

-- Ver lag de cada réplica
SELECT client_addr, state,
       sent_lsn - write_lsn  AS write_lag_bytes,
       write_lsn - flush_lsn AS flush_lag_bytes,
       flush_lsn - replay_lsn AS replay_lag_bytes
FROM pg_stat_replication;
```

```rust
struct ReplicaPool {
    replicas: Vec<ReplicaConn>,
    strategy: ReplicaSelection,
}

enum ReplicaSelection {
    RoundRobin,
    LeastLag,
    LeastConnections,
    Random,
}

impl ReplicaPool {
    async fn read(&self, plan: &PhysicalPlan, max_lag_ms: u64) -> Result<QueryResult> {
        let replica = self.select_replica(max_lag_ms)?;
        replica.execute_readonly(plan).await
    }

    fn select_replica(&self, max_lag_ms: u64) -> Result<&ReplicaConn> {
        let eligible: Vec<_> = self.replicas.iter()
            .filter(|r| r.lag_ms() <= max_lag_ms)
            .collect();

        if eligible.is_empty() {
            return Err(DbError::NoReplicaAvailable { max_lag_ms });
        }

        Ok(match self.strategy {
            ReplicaSelection::LeastLag => {
                eligible.iter().min_by_key(|r| r.lag_ms()).unwrap()
            }
            ReplicaSelection::RoundRobin => {
                &eligible[self.counter.fetch_add(1, Ordering::Relaxed) % eligible.len()]
            }
            _ => &eligible[0],
        })
    }
}
```

---

### Synchronous Commit Configurable

```sql
-- Cuántas réplicas deben confirmar antes de responder OK al cliente
-- Tradeoff: durabilidad vs latencia

SET synchronous_commit = 'off';
-- 0 réplicas — asíncrono total
-- latencia: mínima (~0.1ms)
-- riesgo: perder últimos commits si el primary cae

SET synchronous_commit = 'local';
-- WAL en disco del primary — default
-- latencia: baja (~1ms con SSD)
-- riesgo: perder datos si el primary se cae antes de replicar

SET synchronous_commit = 'remote_write';
-- WAL llegó al SO de la réplica (sin fsync)
-- latencia: depende de la red
-- riesgo: mínimo

SET synchronous_commit = 'remote_apply';
-- La réplica aplicó el cambio y puede servir reads
-- latencia: mayor
-- riesgo: cero pérdida de datos (RPO = 0)

-- Por transacción (override del setting global)
BEGIN;
SET LOCAL synchronous_commit = 'remote_apply';
UPDATE accounts SET balance = balance - 100 WHERE id = 1;
COMMIT;   -- espera hasta que la réplica haya aplicado el cambio
```

```rust
#[derive(Clone, Copy)]
enum SyncCommit { Off, Local, RemoteWrite, RemoteApply }

impl WalWriter {
    async fn commit(&self, txn_id: TxnId, level: SyncCommit) -> Result<()> {
        // Siempre escribir en disco local
        self.flush_local(txn_id)?;

        match level {
            SyncCommit::Off | SyncCommit::Local => Ok(()),

            SyncCommit::RemoteWrite => {
                // Esperar que la réplica haya recibido el WAL (sin fsync)
                self.replication.wait_write(txn_id).await
            }

            SyncCommit::RemoteApply => {
                // Esperar que la réplica haya aplicado el cambio
                self.replication.wait_apply(txn_id).await
            }
        }
    }
}
```

---

### Cascading Replication

```
Sin cascading:    Primary → Réplica1
                  Primary → Réplica2      ← sobrecarga del primary
                  Primary → Réplica3

Con cascading:    Primary → Réplica1 → Réplica2
                                    ↘ Réplica3
                  Primary envía WAL solo a Réplica1.
                  Réplica1 retransmite a Réplica2 y Réplica3.
                  Útil para réplicas en distintas regiones geográficas.
```

```toml
# primary dbyo.toml
[replication]
role     = "primary"
replicas = ["replica1:3306"]   # solo envía a réplica1

# replica1 dbyo.toml
[replication]
role     = "replica"
primary  = "primary:3306"
cascade_to = ["replica2:3306", "replica3:3306"]  # retransmite a estos
```

```rust
struct CascadeReplicator {
    upstream:    Option<WalReceiver>,    // recibe de primary o réplica padre
    downstream:  Vec<WalSender>,         // envía a réplicas hijas
}

impl CascadeReplicator {
    async fn relay(&self) {
        // Recibir del upstream y retransmitir a todos los downstream en paralelo
        while let Some(entry) = self.upstream.as_ref().unwrap().recv().await {
            self.apply_locally(&entry).await;

            // Retransmitir en paralelo a todas las réplicas hijas
            let senders = self.downstream.iter()
                .map(|s| s.send(entry.clone()));
            futures::future::join_all(senders).await;
        }
    }
}
```

---

### Logical Decoding API — WAL como Eventos JSON

```sql
-- Crear slot de decodificación lógica
CREATE REPLICATION SLOT mi_cdc LOGICAL OUTPUT PLUGIN 'dbyo_json';
CREATE REPLICATION SLOT mi_cdc LOGICAL OUTPUT PLUGIN 'wal2json';

-- Consumir cambios como JSON
SELECT lsn, data FROM pg_logical_slot_get_changes('mi_cdc', NULL, NULL);
-- {"action":"I","table":"orders","data":{"id":99,"user_id":1,"total":150.0}}
-- {"action":"U","table":"users","identity":{"id":1},"data":{"nombre":"Ana"}}
-- {"action":"D","table":"sessions","identity":{"id":"abc123"}}

-- Consumir con opciones
SELECT * FROM pg_logical_slot_get_changes('mi_cdc', NULL, 10,
  'include-timestamp', 'true',
  'include-transaction', 'true',
  'format-version', '2'
);

-- Ver posición del slot
SELECT slot_name, restart_lsn, confirmed_flush_lsn, active
FROM pg_replication_slots;

-- El WAL se retiene hasta que el slot confirme haberlo leído
SELECT pg_replication_slot_advance('mi_cdc', '0/1000000');

-- Eliminar slot (libera WAL retenido)
SELECT pg_drop_replication_slot('mi_cdc');
```

```rust
trait LogicalOutputPlugin: Send + Sync {
    fn begin(&self, txn_id: TxnId, timestamp: Timestamp) -> Option<String>;
    fn change(&self, entry: &WalEntry) -> Option<String>;
    fn commit(&self, txn_id: TxnId, lsn: u64) -> Option<String>;
}

// Plugin JSON — formato estándar compatible con Debezium
struct JsonOutputPlugin { pretty: bool, include_timestamp: bool }

impl LogicalOutputPlugin for JsonOutputPlugin {
    fn change(&self, entry: &WalEntry) -> Option<String> {
        let doc = match entry.kind {
            WalKind::Insert => json!({
                "action": "I", "schema": entry.schema,
                "table": entry.table, "data": entry.new_values()
            }),
            WalKind::Update => json!({
                "action": "U", "schema": entry.schema,
                "table": entry.table,
                "identity": entry.old_key_values(),
                "data": entry.new_values()
            }),
            WalKind::Delete => json!({
                "action": "D", "schema": entry.schema,
                "table": entry.table, "identity": entry.old_key_values()
            }),
        };
        Some(if self.pretty { serde_json::to_string_pretty(&doc) }
             else { serde_json::to_string(&doc) }.unwrap())
    }
}
```

---

### DSN Estándar — Connection Strings

```
# Formato dbyo nativo
dbyo://usuario:contraseña@host:3306/base_de_datos?ssl=true&timeout=30s

# Formato PostgreSQL (para compatibilidad con ORMs)
postgres://usuario:contraseña@host:5432/base_de_datos?sslmode=verify-full

# Formato MySQL (para compatibilidad)
mysql://usuario:contraseña@host:3306/base_de_datos?charset=utf8mb4

# Variables de entorno estándar (compatible con todos los ORMs)
DATABASE_URL=postgres://ana:secret@localhost:5432/myapp
DBYO_URL=dbyo://ana:secret@localhost:3306/myapp

# Parámetros soportados
?ssl=true|false|verify-full
?timeout=30s
?connect_timeout=5s
?statement_timeout=60s
?pool_min=2&pool_max=20
?application_name=mi_app
?search_path=ventas,public
?sslcert=/path/to/client.crt&sslkey=/path/to/client.key
```

```rust
#[derive(Debug, Clone)]
struct ConnectionConfig {
    host:               String,
    port:               u16,
    user:               String,
    password:           Option<String>,
    database:           String,
    ssl:                SslMode,
    connect_timeout:    Duration,
    statement_timeout:  Option<Duration>,
    pool_min:           usize,
    pool_max:           usize,
    application_name:   Option<String>,
    search_path:        Vec<String>,
}

impl ConnectionConfig {
    fn from_url(url: &str) -> Result<Self> {
        let parsed = url::Url::parse(url)?;
        Ok(Self {
            host:     parsed.host_str().unwrap_or("localhost").to_string(),
            port:     parsed.port().unwrap_or(3306),
            user:     parsed.username().to_string(),
            password: parsed.password().map(String::from),
            database: parsed.path().trim_start_matches('/').to_string(),
            ssl:      parse_ssl_mode(parsed.query_pairs()),
            connect_timeout: parse_duration(
                parsed.query_pairs(), "connect_timeout", Duration::from_secs(5)
            ),
            pool_max: parse_usize(parsed.query_pairs(), "pool_max", 20),
            ..Default::default()
        })
    }

    fn from_env() -> Result<Self> {
        let url = std::env::var("DATABASE_URL")
            .or_else(|_| std::env::var("DBYO_URL"))
            .map_err(|_| DbError::NoDsnConfigured)?;
        Self::from_url(&url)
    }
}
```

Crate: `url = "2"`.

---

### Extensions System — CREATE EXTENSION

```sql
-- Instalar extensión (plugin SQL + Rust)
CREATE EXTENSION IF NOT EXISTS pg_trgm;         -- trigramas fuzzy
CREATE EXTENSION IF NOT EXISTS vector;           -- pgvector compatible
CREATE EXTENSION IF NOT EXISTS postgis;          -- geospatial
CREATE EXTENSION IF NOT EXISTS uuid_ossp;        -- gen_random_uuid()
CREATE EXTENSION IF NOT EXISTS tablefunc;        -- crosstab, pivot
CREATE EXTENSION IF NOT EXISTS dbyo_ai;          -- funciones AI built-in

-- Listar extensiones
SELECT name, version, description FROM pg_available_extensions ORDER BY name;
SELECT name, installed_version FROM pg_extension;

-- Actualizar extensión
ALTER EXTENSION vector UPDATE TO '0.8';

-- Desinstalar
DROP EXTENSION pg_trgm CASCADE;

-- Crear extensión propia (plugin en Rust + WASM)
CREATE EXTENSION mi_extension
  VERSION '1.0'
  FROM FILE '/extensions/mi_extension.wasm';
```

```rust
struct ExtensionRegistry {
    installed: DashMap<String, Extension>,
    available: Vec<ExtensionManifest>,
}

struct Extension {
    name:        String,
    version:     semver::Version,
    functions:   Vec<Box<dyn SqlFunction>>,
    types:       Vec<CustomType>,
    index_ops:   Vec<IndexOperatorClass>,
    wasm_module: Option<WasmPlugin>,
}

impl ExtensionRegistry {
    fn install(&self, name: &str, engine: &Engine) -> Result<()> {
        let manifest = self.find_available(name)?;
        let ext = manifest.load()?;

        // Registrar funciones de la extensión
        for func in &ext.functions {
            engine.functions.register(func.name(), func.clone());
        }

        // Registrar tipos de la extensión
        for ty in &ext.types {
            engine.catalog.register_type(ty.clone());
        }

        self.installed.insert(name.to_string(), ext);
        Ok(())
    }
}
```

---

### Online VACUUM sin Locks

```sql
-- VACUUM normal: no bloquea reads ni writes (recoge basura MVCC)
VACUUM orders;

-- VACUUM ANALYZE: recoge basura + actualiza estadísticas del planner
VACUUM ANALYZE orders;

-- VACUUM FREEZE: previene Transaction ID Wraparound
-- XIDs son u32 — después de 2B transacciones empieza a dar problemas
VACUUM FREEZE orders;

-- VACUUM FULL: compacta completamente (sí lockea — ACCESS EXCLUSIVE)
VACUUM FULL orders;

-- VACUUM CONCURRENTLY (nuestro diferenciador):
-- Compacta sin bloquear — más lento pero sin downtime
VACUUM CONCURRENTLY orders;

-- Ver progreso de VACUUM en ejecución
SELECT relname, phase, heap_blks_scanned, heap_blks_vacuumed,
       index_vacuum_count, num_dead_tuples
FROM pg_stat_progress_vacuum;

-- Configurar auto-vacuum por tabla
ALTER TABLE orders SET (
  autovacuum_vacuum_scale_factor   = 0.01,  -- vaciar cuando 1% son filas muertas
  autovacuum_analyze_scale_factor  = 0.005, -- analizar cuando 0.5% son nuevas
  autovacuum_vacuum_cost_delay     = 2      -- 2ms de pausa entre páginas (gentil con I/O)
);
```

```rust
struct VacuumWorker {
    engine:    Arc<Engine>,
    config:    VacuumConfig,
}

impl VacuumWorker {
    async fn vacuum_table(&self, table: &str, mode: VacuumMode) -> Result<VacuumStats> {
        let mut stats = VacuumStats::default();

        match mode {
            VacuumMode::Normal => {
                // Escanear páginas buscando filas muertas
                // Sin lock — otros pueden leer y escribir
                for page_id in self.engine.table_pages(table) {
                    let page = self.engine.storage.read_page(page_id)?;
                    let dead = self.collect_dead_rows(&page)?;

                    if !dead.is_empty() {
                        // Reescribir página sin las filas muertas
                        // Breve lock de página (microsegundos)
                        self.engine.storage.rewrite_page(page_id, &dead)?;
                        stats.dead_rows_removed += dead.len() as u64;
                    }

                    // Pausa configurable para no saturar I/O
                    if self.config.cost_delay > 0 {
                        tokio::time::sleep(Duration::from_millis(self.config.cost_delay)).await;
                    }
                }
            }

            VacuumMode::Concurrent => {
                // Igual que Normal pero con pausa más agresiva
                // Permite que otras queries adelanten
                self.config.cost_delay = 10; // 10ms entre páginas
                self.vacuum_table(table, VacuumMode::Normal).await?;
            }

            VacuumMode::Full => {
                // Lock exclusivo + reescribir toda la tabla compactada
                let _lock = self.engine.locks.acquire_table(
                    table, LockMode::AccessExclusive
                ).await?;
                self.compact_full(table, &mut stats).await?;
            }
        }
        Ok(stats)
    }
}
```

---

### Parallel DDL

```sql
-- CREATE TABLE AS SELECT con paralelismo
CREATE TABLE orders_2025
  PARALLEL 8 AS
  SELECT * FROM orders WHERE YEAR(created_at) = 2025;
-- 8 workers leen orders en paralelo y escriben orders_2025

-- CREATE INDEX en paralelo (además de CONCURRENTLY)
CREATE INDEX PARALLEL 4 idx_total ON orders(total);

-- REFRESH MATERIALIZED VIEW en paralelo
REFRESH MATERIALIZED VIEW CONCURRENTLY ventas_mensuales WITH PARALLEL 4;

-- Clonar tabla en paralelo (útil para backups lógicos)
CREATE TABLE orders_backup (LIKE orders INCLUDING ALL)
  AS SELECT * FROM orders
  WITH PARALLEL 8;

-- Ver workers DDL activos
SELECT pid, phase, blocks_done, blocks_total
FROM pg_stat_progress_create_index;
```

```rust
struct ParallelDdlExecutor {
    pool: rayon::ThreadPool,
}

impl ParallelDdlExecutor {
    async fn create_table_as(
        &self,
        name: &str,
        query: &PhysicalPlan,
        parallelism: usize,
        engine: &Engine,
    ) -> Result<u64> {
        // Dividir el scan source en morsels
        let morsels = engine.storage.split_morsels(query.source_table(), parallelism);

        // Crear tabla destino vacía
        engine.catalog.create_table(name, query.output_schema())?;

        // Ejecutar en paralelo con Rayon
        let rows_written = Arc::new(AtomicU64::new(0));
        let rw = rows_written.clone();

        self.pool.install(|| {
            morsels.par_iter().for_each(|morsel| {
                let rows = engine.executor.execute_morsel(query, morsel).unwrap();
                let n = rows.len() as u64;
                engine.storage.bulk_insert(name, rows).unwrap();
                rw.fetch_add(n, Ordering::Relaxed);
            });
        });

        Ok(rows_written.load(Ordering::Relaxed))
    }
}
```

---

## Benchmark objetivo

```
Condiciones: 1M registros, 16 threads concurrentes, SSD NVMe

Operación              Nuestra BD    MySQL 8.0    Speedup
─────────────────────────────────────────────────────────
Point lookup (PK)      800k ops/s    350k ops/s    ~2.3x
Range scan 10k rows    45ms          120ms         ~2.7x
Seq scan 1M rows       0.8s          3.4s          ~4.2x
INSERT con WAL         180k ops/s    95k ops/s     ~1.9x
Concurrent reads x16   lineal        se satura     ~3x+
JOIN simple (FK idx)   60k rows/s    40k rows/s    ~1.5x
```

**Dónde ganamos claramente:**
- Lecturas concurrentes (no hay locks, CoW B+ Tree)
- Table scans (SIMD AVX2 vectorizado + morsel parallelism)
- Queries con alta selectividad (late materialization)
- Pipelines simples (operator fusion elimina buffers)
- Punto de lookup cuando el árbol cabe en cache del CPU

**Dónde MySQL es competitivo:**
- Query optimizer para queries complejas (años de trabajo)
- Workloads muy variados simultáneos

---

## Observabilidad Avanzada — Estadísticas de Uso

### pg_stat_user_tables — acceso por tabla

```sql
SELECT relname AS tabla,
  seq_scan,                           -- full scans (alto = falta índice)
  idx_scan,                           -- index scans
  n_live_tup,                         -- filas vivas
  n_dead_tup,                         -- filas muertas (necesitan VACUUM)
  n_dead_tup::float / NULLIF(n_live_tup + n_dead_tup, 0) AS dead_ratio,
  last_vacuum, last_autovacuum,
  last_analyze, last_autoanalyze
FROM pg_stat_user_tables
ORDER BY n_dead_tup DESC;

-- Alertar cuando dead_ratio > 10%
SELECT relname FROM pg_stat_user_tables
WHERE n_dead_tup::float / NULLIF(n_live_tup + n_dead_tup, 0) > 0.10;
```

### pg_stat_user_indexes — uso por índice

```sql
-- Ver índices que NUNCA se usan → candidatos a eliminar
SELECT indexrelname AS indice,
  relname AS tabla,
  idx_scan,                           -- 0 = nunca usado
  pg_size_pretty(pg_relation_size(indexrelid)) AS tamaño
FROM pg_stat_user_indexes
WHERE idx_scan = 0
ORDER BY pg_relation_size(indexrelid) DESC;

-- Ver los índices más usados
SELECT indexrelname, idx_scan, idx_tup_read, idx_tup_fetch
FROM pg_stat_user_indexes
ORDER BY idx_scan DESC LIMIT 10;
```

### Table/Index Bloat Detection

```sql
-- Tablas con alta ratio de filas muertas
SELECT relname, n_live_tup, n_dead_tup,
  ROUND(n_dead_tup::numeric / NULLIF(n_live_tup + n_dead_tup, 0) * 100, 1) AS dead_pct
FROM pg_stat_user_tables
WHERE n_dead_tup > 1000
ORDER BY dead_pct DESC;

-- Acción automática: si dead_pct > 20%, disparar VACUUM
SELECT CASE
  WHEN dead_pct > 20 THEN 'VACUUM ' || relname
  WHEN dead_pct > 10 THEN 'VACUUM ANALYZE ' || relname
  ELSE '-- OK: ' || relname
END AS accion
FROM (
  SELECT relname,
    ROUND(n_dead_tup::numeric / NULLIF(n_live_tup + n_dead_tup, 0) * 100, 1) AS dead_pct
  FROM pg_stat_user_tables
) t ORDER BY dead_pct DESC;
```

---

## SQLSTATE — Códigos de Error Estándar SQL

Necesarios para compatibilidad con ORMs (Hibernate, SQLAlchemy, Prisma, etc.)
que inspeccionan el código de error para decidir si reintentar o no.

```rust
#[derive(Debug, Clone, Copy, PartialEq)]
enum SqlState {
    // Clase 00 — Éxito
    Success                    = 0x00000,

    // Clase 08 — Errores de conexión
    ConnectionException        = 0x08000,
    ConnectionDoesNotExist     = 0x08003,
    ConnectionFailure          = 0x08006,

    // Clase 22 — Datos inválidos
    DataException              = 0x22000,
    NumericValueOutOfRange     = 0x22003,
    InvalidDatetimeFormat      = 0x22007,
    StringDataRightTruncation  = 0x22001,
    DivisionByZero             = 0x22012,
    InvalidEscapeSequence      = 0x22025,

    // Clase 23 — Violaciones de integridad
    IntegrityConstraintViolation = 0x23000,
    NotNullViolation           = 0x23502,
    ForeignKeyViolation        = 0x23503,
    UniqueViolation            = 0x23505,  // el más común — email duplicado
    CheckViolation             = 0x23514,

    // Clase 25 — Estado de transacción inválido
    ActiveSqlTransaction       = 0x25001,
    ReadOnlySqlTransaction     = 0x25006,

    // Clase 26 — Nombre de statement inválido
    InvalidSqlStatementName    = 0x26000,

    // Clase 28 — Autorización
    InvalidAuthorizationSpec   = 0x28000,
    InvalidPassword            = 0x28P01,

    // Clase 40 — Rollback de transacción
    TransactionRollback        = 0x40000,
    SerializationFailure       = 0x40001,  // SSI detectó conflicto
    DeadlockDetected           = 0x40P01,

    // Clase 42 — Errores de sintaxis
    SyntaxError                = 0x42601,
    UndefinedTable             = 0x42P01,
    UndefinedColumn            = 0x42703,
    UndefinedFunction          = 0x42883,
    DuplicateTable             = 0x42P07,
    DuplicateColumn            = 0x42701,
    InsufficientPrivilege      = 0x42501,

    // Clase 53 — Recursos insuficientes
    InsufficientResources      = 0x53000,
    TooManyConnections         = 0x53300,

    // Clase 57 — Intervención del operador
    StatementTimeout           = 0x57014,
    LockTimeout                = 0x55P03,
    QueryCanceled              = 0x57014,
}

impl DbError {
    fn sqlstate(&self) -> SqlState {
        match self {
            DbError::UniqueViolation { .. }  => SqlState::UniqueViolation,
            DbError::ForeignKeyViolation { .. } => SqlState::ForeignKeyViolation,
            DbError::NotNull { .. }          => SqlState::NotNullViolation,
            DbError::CheckViolation { .. }   => SqlState::CheckViolation,
            DbError::Deadlock { .. }         => SqlState::DeadlockDetected,
            DbError::StatementTimeout        => SqlState::StatementTimeout,
            DbError::LockTimeout             => SqlState::LockTimeout,
            DbError::ParseError { .. }       => SqlState::SyntaxError,
            DbError::TableNotFound { .. }    => SqlState::UndefinedTable,
            DbError::ColumnNotFound { .. }   => SqlState::UndefinedColumn,
            DbError::PermissionDenied { .. } => SqlState::InsufficientPrivilege,
            _                                => SqlState::DataException,
        }
    }
}
```

---

## Deployment — Fase 35

### Dockerfile

```dockerfile
# Multi-stage: compilar en imagen completa, correr en imagen mínima
FROM rust:1.80-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY crates/ ./crates/
RUN cargo build --release --bin dbyo-server

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y libssl3 ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd -ms /bin/bash dbyo
COPY --from=builder /app/target/release/dbyo-server /usr/local/bin/
USER dbyo
EXPOSE 3306 5432
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=5s \
  CMD dbyo-server --healthcheck || exit 1
CMD ["dbyo-server", "--data-dir", "/data", "--config", "/etc/dbyo/dbyo.toml"]
```

### docker-compose.yml

```yaml
version: '3.9'
services:
  dbyo:
    image: dbyo:latest
    build: .
    ports:
      - "3306:3306"   # MySQL protocol
      - "5432:5432"   # PostgreSQL protocol
    volumes:
      - dbyo_data:/data
      - ./dbyo.toml:/etc/dbyo/dbyo.toml:ro
    environment:
      DBYO_PASSWORD: ${DBYO_PASSWORD:-secret}
      DBYO_DATABASE: ${DBYO_DATABASE:-myapp}
      DBYO_LOG_LEVEL: ${DBYO_LOG_LEVEL:-info}
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "dbyo-server", "--healthcheck"]
      interval: 30s
      timeout: 5s
      retries: 3

volumes:
  dbyo_data:
    driver: local
```

### dbyo.toml — configuración completa

```toml
[server]
host              = "0.0.0.0"
mysql_port        = 3306
postgres_port     = 5432
max_connections   = 200
default_database  = "myapp"
data_dir          = "/data"

[auth]
default_password_hash = "argon2id"
max_auth_failures     = 5
lockout_duration      = "15m"

[[auth.rules]]             # equivalente pg_hba.conf
host     = "127.0.0.1/32"
user     = "all"
method   = "scram-sha-256"

[[auth.rules]]
host     = "0.0.0.0/0"
user     = "all"
method   = "reject"

[tls]
enabled          = true
cert             = "/etc/dbyo/server.crt"
key              = "/etc/dbyo/server.key"
min_version      = "TLS1.3"

[storage]
page_size        = 8192       # bytes
mmap_size        = "4GB"      # max tamaño del mmap
checkpoint_every = "5min"
fsync            = true

[memory]
query_cache_size = "256MB"
max_memory       = "8GB"      # límite total de RAM
lru_eviction     = true

[timeouts]
statement_timeout              = "60s"
lock_timeout                   = "10s"
deadlock_timeout               = "1s"
idle_in_transaction_timeout    = "30min"
connection_idle_timeout        = "10min"

[logging]
level            = "info"     # trace | debug | info | warn | error
format           = "json"     # json | text
file             = "/var/log/dbyo/dbyo.log"
rotation         = "daily"    # daily | hourly | size:100MB
keep_days        = 30
slow_query_ms    = 100        # loggear queries más lentas que esto
log_connections  = true

[vacuum]
autovacuum              = true
vacuum_scale_factor     = 0.01   # vacuar cuando 1% son dead tuples
analyze_scale_factor    = 0.005
vacuum_cost_delay       = "2ms"

[ai]
provider         = "ollama"
embed_model      = "nomic-embed-text"
llm_model        = "llama3.1"
endpoint         = "http://localhost:11434"

[replication]
role             = "primary"   # primary | replica
sync_replicas    = 1
wal_retention    = "7d"
```

### systemd service

```ini
# /etc/systemd/system/dbyo.service
[Unit]
Description=dbyo Database Server
Documentation=https://github.com/usuario/dbyo
After=network.target
Wants=network.target

[Service]
Type=simple
User=dbyo
Group=dbyo
ExecStart=/usr/local/bin/dbyo-server --config /etc/dbyo/dbyo.toml
ExecReload=/bin/kill -HUP $MAINPID
Restart=always
RestartSec=5
LimitNOFILE=65536
LimitNPROC=4096
PrivateTmp=true
ProtectSystem=full
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
```

### dbyo-client — SDK oficial Rust

```toml
# En tu app:
[dependencies]
dbyo-client = "0.1"
```

```rust
use dbyo_client::{Pool, Config, Row};

// Pool de conexiones
let pool = Pool::builder()
    .config(Config::from_url("dbyo://user:pass@localhost/myapp")?)
    .max_connections(20)
    .build()
    .await?;

// Query con tipos
let rows: Vec<Row> = pool
    .query("SELECT id, nombre FROM users WHERE id = $1", &[&42])
    .await?;

// Tipado fuerte con derive
#[derive(dbyo_client::FromRow)]
struct User { id: i32, nombre: String }

let user: User = pool.query_one("SELECT * FROM users WHERE id = $1", &[&1]).await?;

// Transacción
let mut txn = pool.begin().await?;
txn.execute("UPDATE accounts SET balance = balance - $1 WHERE id = $2", &[&100, &1]).await?;
txn.execute("UPDATE accounts SET balance = balance + $1 WHERE id = $2", &[&100, &2]).await?;
txn.commit().await?;
```

### dbyo-bench — herramienta de carga

```bash
# Instalar
cargo install dbyo-bench

# Escenario OLTP point select (equivalente sysbench)
dbyo-bench oltp_point_select \
  --host localhost --port 3306 \
  --user root --database test \
  --tables 1 --table-size 1000000 \
  --clients 16 --threads 4 \
  --time 60

# Comparar vs MySQL
dbyo-bench compare \
  --dbyo "dbyo://root@localhost:3306/test" \
  --mysql "mysql://root@localhost:3307/test" \
  --scenario oltp_read_write \
  --time 60

# Resultado:
# ┌──────────────────┬──────────────┬──────────────┬──────────┐
# │ Operación        │ dbyo         │ MySQL 8.0    │ Delta    │
# ├──────────────────┼──────────────┼──────────────┼──────────┤
# │ Point lookup     │ 823k ops/s   │ 347k ops/s   │ +137%    │
# │ Range scan 10K   │ 43ms         │ 118ms        │ -64%     │
# │ INSERT+WAL       │ 184k ops/s   │ 98k ops/s    │ +88%     │
# │ Concurrent x16   │ 3.1M ops/s   │ 1.2M ops/s   │ +158%    │
# └──────────────────┴──────────────┴──────────────┴──────────┘
```

---

## Inspiración en ISAM — evolución del concepto

| Concepto ISAM original | Nuestra versión moderna |
|---|---|
| Bloques fijos en disco | Páginas 8KB con mmap |
| Índice estático (no crece dinámicamente) | B+ Tree dinámico (split/merge) |
| Áreas de overflow separadas | WAL + versiones en el árbol (CoW) |
| Acceso secuencial + por índice | Igual + range scans + SIMD |
| Sin transacciones | MVCC completo |
| Sin concurrencia real | Readers lockless, writers serializados |
| Sin FK | FK con índice inverso |

---

## Referencias

- **LMDB** (Lightning Memory-Mapped Database) — arquitectura mmap + CoW B+ Tree
- **WiredTiger** (motor de MongoDB) — B+ Tree + prefix compression
- **DuckDB** — vectorized execution, morsel parallelism, operator fusion
- **PostgreSQL** — partial indexes, TOAST, EXPLAIN ANALYZE, MVCC
- **RocksDB** — bloom filters, LSM + compaction
- **ClickHouse** — sparse indexes, columnar storage
- **SQLite** — in-memory mode, JSON, single-file embedded
- **FoundationDB** — deterministic simulation testing
- **HyPer / Umbra** — JIT compilation con LLVM
- **TimescaleDB** — particionamiento por tiempo, compresión histórica, continuous aggregates
- **Redis** — TTL por fila, LRU eviction
- **MongoDB** — Change Streams / CDC basado en WAL
- **DoltDB** — git versioning para datos (branches, commits, merge)
- **Apache Arrow** — formato columnar para resultados analíticos
- **PlanetScale / gh-ost** — non-blocking schema changes
- **PgBouncer** — connection pooling integrado
- **CMU 15-445** — curso de bases de datos con implementación práctica
- **"Database Internals"** (Alex Petrov) — libro de referencia para implementación
- **go-mysql-server** (DoltHub) — referencia de parser SQL en lenguaje moderno
