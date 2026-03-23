# Data Types

NexusDB implements a rich type system that covers the common SQL standard types as well
as several extensions for modern workloads (UUID, JSON, VECTOR for AI embeddings, RANGE
types for temporal and numeric overlaps).

---

## Integer Types

| SQL Type   | Aliases        | Storage | Rust type | Range                                    |
|------------|----------------|---------|-----------|------------------------------------------|
| `BOOL`     | `BOOLEAN`      | 1 byte  | `bool`    | TRUE / FALSE                             |
| `TINYINT`  | `INT1`         | 1 byte  | `i8`      | -128 to 127                              |
| `UTINYINT` | `UINT1`        | 1 byte  | `u8`      | 0 to 255                                 |
| `SMALLINT` | `INT2`         | 2 bytes | `i16`     | -32,768 to 32,767                        |
| `USMALLINT`| `UINT2`        | 2 bytes | `u16`     | 0 to 65,535                              |
| `INT`      | `INTEGER, INT4`| 4 bytes | `i32`     | -2,147,483,648 to 2,147,483,647          |
| `UINT`     | `UINT4`        | 4 bytes | `u32`     | 0 to 4,294,967,295                       |
| `BIGINT`   | `INT8`         | 8 bytes | `i64`     | -9.2 × 10¹⁸ to 9.2 × 10¹⁸               |
| `UBIGINT`  | `UINT8`        | 8 bytes | `u64`     | 0 to 18.4 × 10¹⁸ (used for LSN, page_id)|
| `HUGEINT`  | `INT16`        | 16 bytes| `i128`    | ±1.7 × 10³⁸ (cryptography, checksums)   |

```sql
-- Typical primary key
CREATE TABLE users (
    id   BIGINT PRIMARY KEY AUTO_INCREMENT,
    age  SMALLINT NOT NULL
);

-- Unsigned counter that never goes negative
CREATE TABLE page_views (
    page_id  INT  NOT NULL,
    views    UINT NOT NULL DEFAULT 0
);
```

---

## Floating-Point Types

| SQL Type | Aliases                         | Storage | Rust type | Notes                             |
|----------|---------------------------------|---------|-----------|-----------------------------------|
| `REAL`   | `FLOAT4`, `FLOAT`               | 4 bytes | `f32`     | Coordinates, ratings, embeddings  |
| `DOUBLE` | `FLOAT8`, `DOUBLE PRECISION`    | 8 bytes | `f64`     | Scientific calculations           |

> **NaN is forbidden.** The row codec rejects `NaN` values at encode time.
> IEEE 754 infinities are also not accepted by default.

```sql
-- Geospatial coordinates (4-byte precision is sufficient)
CREATE TABLE locations (
    id   INT   PRIMARY KEY,
    lat  REAL  NOT NULL,
    lon  REAL  NOT NULL
);

-- Scientific measurements requiring high precision
CREATE TABLE experiments (
    id      INT    PRIMARY KEY,
    result  DOUBLE NOT NULL
);
```

---

## Exact Numeric — DECIMAL

| SQL Type         | Aliases           | Storage  | Rust type | Notes                         |
|------------------|-------------------|----------|-----------|-------------------------------|
| `DECIMAL(p, s)`  | `NUMERIC(p, s)`   | 17 bytes | `i128` + `u8` scale | Exact arithmetic, no float error |

**Always use `DECIMAL` for money.** Floating-point types cannot represent
`0.1 + 0.2` exactly; `DECIMAL` always can.

```sql
CREATE TABLE invoices (
    id       BIGINT       PRIMARY KEY AUTO_INCREMENT,
    subtotal DECIMAL      NOT NULL,    -- DECIMAL without precision = DECIMAL(38,0)
    tax_rate DECIMAL      NOT NULL,
    total    DECIMAL      NOT NULL
);

-- Insert with exact values
INSERT INTO invoices (subtotal, tax_rate, total)
VALUES (199.99, 0.19, 237.99);

-- Arithmetic is always exact
SELECT subtotal * tax_rate AS computed_tax FROM invoices WHERE id = 1;
-- Returns: 37.9981  (never 37.99809999999...)
```

The internal codec stores `DECIMAL` as a 16-byte little-endian `i128` mantissa followed
by a 1-byte scale (total 17 bytes per non-NULL value).

---

## Text Types

| SQL Type       | Aliases | Max length        | Rust type   | Notes                          |
|----------------|---------|-------------------|-------------|--------------------------------|
| `CHAR(n)`      |         | n bytes (fixed)   | `[u8; n]`   | Right-padded with spaces       |
| `VARCHAR(n)`   |         | n bytes (max)     | `String`    | Variable, UTF-8                |
| `TEXT`         |         | 16,777,215 bytes  | `String`    | Unlimited (TOAST if >16 KB)    |
| `CITEXT`       |         | 16,777,215 bytes  | `String`    | Case-insensitive comparison    |

The codec encodes `TEXT` and `VARCHAR` with a 3-byte (u24) length prefix followed by
raw UTF-8 bytes. This limits inline storage to 16,777,215 bytes; values larger than a
page use TOAST (planned Phase 6).

```sql
-- Fixed-length codes (ISO country, state abbreviations)
CREATE TABLE countries (
    code  CHAR(2)      PRIMARY KEY,   -- 'US', 'DE', 'JP'
    name  VARCHAR(128) NOT NULL
);

-- Unlimited text content
CREATE TABLE blog_posts (
    id      BIGINT PRIMARY KEY AUTO_INCREMENT,
    title   VARCHAR(512) NOT NULL,
    body    TEXT         NOT NULL
);

-- Case-insensitive email lookup
CREATE TABLE users (
    id    BIGINT PRIMARY KEY AUTO_INCREMENT,
    email CITEXT NOT NULL UNIQUE
);
-- SELECT * FROM users WHERE email = 'ALICE@EXAMPLE.COM'
-- matches rows where email = 'alice@example.com'
```

---

## Binary Type

| SQL Type | Aliases       | Max length       | Rust type  | Notes                   |
|----------|---------------|------------------|------------|-------------------------|
| `BYTEA`  | `BLOB`, `BYTES` | 16,777,215 bytes | `Vec<u8>` | Raw bytes, hex display  |

```sql
CREATE TABLE attachments (
    id      BIGINT PRIMARY KEY AUTO_INCREMENT,
    name    TEXT   NOT NULL,
    content BYTEA  NOT NULL
);

-- Insert binary with hex literal
INSERT INTO attachments (name, content) VALUES ('icon.png', X'89504e47');

-- Display as hex
SELECT name, encode(content, 'hex') FROM attachments;
```

---

## Date and Time Types

| SQL Type       | Storage  | Internal repr    | Notes                                     |
|----------------|----------|------------------|-------------------------------------------|
| `DATE`         | 4 bytes  | `i32` days since 1970-01-01 | No time component              |
| `TIME`         | 8 bytes  | `i64` µs since midnight     | No timezone                    |
| `TIMETZ`       | 12 bytes | `i64` µs + `i32` offset     | Time with timezone offset      |
| `TIMESTAMP`    | 8 bytes  | `i64` µs since UTC epoch    | Without timezone (ambiguous)   |
| `TIMESTAMPTZ`  | 8 bytes  | `i64` µs UTC                | **Recommended.** Always UTC internally |
| `INTERVAL`     | 16 bytes | `i32` months + `i32` days + `i64` µs | Correct calendar arithmetic |

> **Prefer `TIMESTAMPTZ` over `TIMESTAMP`.** Without a timezone, there is no way to
> determine the absolute instant when the server and client are in different timezones.
> `TIMESTAMPTZ` stores everything as UTC and converts on display.

```sql
CREATE TABLE events (
    id          BIGINT      PRIMARY KEY AUTO_INCREMENT,
    title       TEXT        NOT NULL,
    starts_at   TIMESTAMPTZ NOT NULL,
    ends_at     TIMESTAMPTZ NOT NULL,
    duration    INTERVAL
);

INSERT INTO events (title, starts_at, ends_at, duration)
VALUES (
    'Team meeting',
    '2026-03-21 10:00:00+00',
    '2026-03-21 11:00:00+00',
    '1 hour'
);
```

### INTERVAL — Calendar-Correct Arithmetic

`INTERVAL` separates months, days, and microseconds because they are not fixed durations:
- "1 month" added to January 31 gives February 28 (or 29).
- "1 day" during a DST transition can be 23 or 25 hours.

```sql
-- Add 1 month to a date (calendar-aware)
SELECT '2026-01-31'::DATE + INTERVAL '1 month';  -- 2026-02-28

-- Add 30 days (fixed)
SELECT '2026-01-31'::DATE + INTERVAL '30 days';  -- 2026-03-02
```

---

## UUID

| SQL Type | Storage  | Notes                                    |
|----------|----------|------------------------------------------|
| `UUID`   | 16 bytes | Stored as raw 16 bytes, displayed as hex |

```sql
CREATE TABLE sessions (
    id         UUID   PRIMARY KEY DEFAULT gen_uuid_v7(),
    user_id    BIGINT NOT NULL,
    created_at TIMESTAMPTZ NOT NULL
);
```

**UUID v7 vs v4 as Primary Key:**

| Strategy | Insert rate (1M rows) | Reason                                   |
|----------|-----------------------|------------------------------------------|
| UUID v4  | ~150k inserts/s       | Random → many B+ Tree page splits        |
| UUID v7  | ~250k inserts/s       | Time-ordered prefix → nearly sequential  |
| BIGINT   | ~280k inserts/s       | Fully sequential                         |

For new schemas, prefer UUID v7 (time-sortable) or BIGINT AUTO_INCREMENT.

---

## Network Types

| SQL Type  | Storage  | Notes                       |
|-----------|----------|-----------------------------|
| `INET`    | 16 bytes | IPv4 or IPv6 address        |
| `CIDR`    | 17 bytes | IP network with prefix mask |
| `MACADDR` | 6 bytes  | MAC address                 |

```sql
CREATE TABLE access_log (
    id         BIGINT PRIMARY KEY AUTO_INCREMENT,
    client_ip  INET   NOT NULL,
    network    CIDR,
    mac        MACADDR
);
```

---

## JSON / JSONB

| SQL Type | Aliases | Notes                                        |
|----------|---------|----------------------------------------------|
| `JSON`   | `JSONB` | Stored as serialized JSON; TOAST if > 2 KB   |

```sql
CREATE TABLE api_responses (
    id       BIGINT PRIMARY KEY AUTO_INCREMENT,
    endpoint TEXT   NOT NULL,
    payload  JSON   NOT NULL
);

INSERT INTO api_responses (endpoint, payload)
VALUES ('/users', '{"count": 42, "items": []}');
```

---

## VECTOR — AI Embeddings

| SQL Type    | Storage  | Notes                                    |
|-------------|----------|------------------------------------------|
| `VECTOR(n)` | `4n` bytes | Array of `n` 32-bit floats (f32)       |

```sql
-- Store sentence embeddings from an AI model
CREATE TABLE documents (
    id        BIGINT      PRIMARY KEY AUTO_INCREMENT,
    content   TEXT        NOT NULL,
    embedding VECTOR(384) NOT NULL   -- e.g. all-MiniLM-L6-v2 output
);

-- Approximate nearest-neighbor search (ANN index required)
SELECT id, content
FROM documents
ORDER BY embedding <-> '[0.12, 0.34, ...]'::vector
LIMIT 10;
```

---

## RANGE Types

RANGE types represent a continuous span of a base type, with inclusive/exclusive
bounds. They support containment (`@>`), overlapping (`&&`), and
exclusion constraints.

| SQL Type      | Base type   | Example                      |
|---------------|-------------|------------------------------|
| `INT4RANGE`   | `INT`       | `[1, 100)`                   |
| `INT8RANGE`   | `BIGINT`    | `[1000, 9999]`               |
| `DATERANGE`   | `DATE`      | `[2026-01-01, 2026-12-31]`   |
| `TSRANGE`     | `TIMESTAMP` | `[2026-01-01 09:00, ...)`    |
| `TSTZRANGE`   | `TIMESTAMPTZ` | timezone-aware variant     |

```sql
-- Prevent overlapping reservations using an exclusion constraint
CREATE TABLE room_reservations (
    room_id   INT     NOT NULL,
    period    TSRANGE NOT NULL,
    EXCLUDE USING gist(room_id WITH =, period WITH &&)
);

INSERT INTO room_reservations VALUES (1, '[2026-03-21 09:00, 2026-03-21 11:00)');
-- This next insert fails: the period overlaps with the existing row
INSERT INTO room_reservations VALUES (1, '[2026-03-21 10:00, 2026-03-21 12:00)');
-- ERROR: exclusion constraint violation
```

---

## NULL in Every Type

Every column of every type can hold NULL unless declared `NOT NULL`. The row codec
stores a compact null bitmap at the start of each row (1 bit per column), so NULL
costs only 1 bit of overhead regardless of the underlying type size.

```sql
SELECT NULL + 5;         -- NULL  (any arithmetic with NULL propagates NULL)
SELECT NULL = NULL;      -- NULL  (not TRUE — use IS NULL instead)
SELECT NULL IS NULL;     -- TRUE
SELECT COALESCE(NULL, 0); -- 0   (return first non-NULL argument)
```

See [Expressions & Operators](expressions.md) for the full NULL semantics table.
