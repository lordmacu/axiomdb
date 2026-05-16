# Data Types

AxiomDB implements a rich type system that covers the common SQL standard types as well
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
page use Phase 11.2 TOAST.

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

AxiomDB stores oversized `TEXT`, `JSON`, and `BYTEA` values out-of-line with
TOAST. Inserts keep a fixed inline pointer in the row and place the large value
in overflow pages. Deletes release the referenced overflow chain through the
refcount-aware BLOB path introduced in Phase 11.2d.

<div class="callout callout-tip">
<span class="callout-icon">💡</span>
<div class="callout-body">
<span class="callout-label">Usage Tip — Large Values</span>
Use `BYTEA`/`BLOB` directly for attachments or binary payloads. Values above the TOAST threshold are still selected as normal `Vec<u8>` results; the overflow pointer is internal and does not change SQL.
</div>
</div>

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

## Enum Types

`CREATE TYPE name AS ENUM (...)` creates a schema-scoped enum type. Enum
columns are stored with the existing `TEXT` row codec but keep their declared
type in the catalog, so writes are validated against the enum label list and
metadata reports the enum name.

```sql
CREATE TYPE mood AS ENUM ('sad', 'ok', 'happy');

CREATE TABLE tasks (
    id    BIGINT PRIMARY KEY AUTO_INCREMENT,
    state mood NOT NULL
);

INSERT INTO tasks (state) VALUES ('ok');     -- accepted
INSERT INTO tasks (state) VALUES ('angry');  -- rejected
```

`SHOW COLUMNS`, `SHOW CREATE TABLE`, and `information_schema.COLUMNS` display
the declared enum type, for example `public.mood`, instead of the physical
`TEXT` representation.

**Dropping an enum type:**

```sql
DROP TYPE public.mood;
DROP TYPE IF EXISTS public.mood;
```

Current limits: `ALTER TYPE ... ADD VALUE` and enum-specific
declaration-order comparison are not implemented yet.

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

## JSON and JSONB

| SQL Type | Aliases | Storage | Notes |
|----------|---------|---------|-------|
| `JSON`   | —       | u24 length + UTF-8 bytes | Validated JSON text; TOAST handles oversized values |
| `JSONB`  | —       | Binary (JEntry format) | O(1) key access, no re-parse on read |

AxiomDB offers two JSON types. Use `JSON` for values written infrequently and read
as whole documents. Use `JSONB` for values where you extract fields frequently —
`JSONB` stores data in a binary format that allows key lookup and JSONPath evaluation
without re-parsing the JSON text on every read.

```sql
CREATE TABLE api_responses (
    id       BIGINT PRIMARY KEY AUTO_INCREMENT,
    endpoint TEXT   NOT NULL,
    payload  JSONB  NOT NULL
);

INSERT INTO api_responses (endpoint, payload)
VALUES ('/users', '{"count": 42, "items": []}');

-- Key extraction with -> (returns typed value or sub-document)
SELECT payload->'count' FROM api_responses;         -- 42

-- Text extraction with ->>
SELECT payload->>'count' FROM api_responses;        -- "42" (TEXT)

-- JSONPath
SELECT JSON_PATH_QUERY_FIRST(payload, '$.count') FROM api_responses; -- 42

-- JSON_EXTRACT also works on JSONB
SELECT JSON_EXTRACT(payload, '$.count') FROM api_responses; -- 42
```

If the payload is malformed, fix the JSON text before inserting:

```sql
INSERT INTO api_responses VALUES (1, '/bad', '{count: 42}');
-- ERROR 22P02: invalid value: invalid JSON: key must be a string
```

### JSONB Binary Format

JSONB stores JSON values in a compact binary format (PostgreSQL-inspired JEntry layout):

- **Container header** (4 bytes): bit 31 = 1 for arrays, bits 30..0 = element count
- **JEntry array**: one `u32` per element encoding type + length/offset
- **Data section**: key strings sorted bytewise-length-first (enables binary search), then value payloads
- **Stride-32 optimization**: every 32nd JEntry stores an absolute offset → `O(1)` random access to any element

Key extraction (`data->'name'`) performs a binary search over the sorted key section —
zero heap allocation for the lookup.

<div class="callout callout-advantage">
<span class="callout-icon">⚡</span>
<div class="callout-body">
<span class="callout-label">Advantage — Zero-Allocation Key Lookup</span>
<code>JsonbRef::get_key()</code> binary-searches the sorted key section without any heap allocation. PostgreSQL <code>findJsonbValueFromContainer()</code> does the same but only after a separate parse phase; AxiomDB stores data pre-sorted at write time so every read is O(log k) with k = number of keys.
</div>
</div>

### JSON Functions

All JSON functions accept both `JSON` (text-backed) and `JSONB` (binary) values:

| Function | Description |
|----------|-------------|
| `JSON_EXTRACT(doc, path)` | Extract value at `$.key`, `$[0]`, nested paths |
| `JSON_SET(doc, path, val)` | Set value at path |
| `JSON_REMOVE(doc, path)` | Remove value at path |
| `JSON_KEYS(doc)` | Return object keys as JSON array |
| `JSON_TYPE(doc)` | Return `'OBJECT'`, `'ARRAY'`, `'STRING'`, etc. |
| `JSON_VALID(doc)` | Return 1 if valid JSON, 0 otherwise |
| `JSON_MERGE_PATCH(a, b)` | RFC 7396 merge: `b` overwrites keys in `a` |
| `JSON_CONTAINS(doc, query)` | Return 1 if `query` is a subset of `doc` |
| `JSON_OVERLAPS(a, b)` | Return 1 if `a` and `b` share any element |
| `JSON_ARRAY_LENGTH(arr)` | Length of array; NULL if not an array |
| `JSON_DEPTH(val)` | Maximum nesting depth |
| `JSON_PRETTY(val)` | Formatted JSON text with indentation |
| `TO_JSONB(text)` | Convert JSON text to binary JSONB |

### JSONPath

`JSONB` supports SQL:2016 JSONPath expressions:

| Expression | Meaning |
|------------|---------|
| `$` | Root node |
| `$.key` | Object field |
| `$[0]` | Array element at index |
| `$.*` | All object values |
| `$[*]` | All array elements |
| `$..key` | Recursive descent — all `key` fields at any depth |
| `$[?(@.price > 10)]` | Filter: array elements matching predicate |

```sql
-- Find all orders over $100
SELECT JSON_PATH_QUERY(payload, '$[?(@.total > 100)]') FROM api_responses;

-- Get first tag from each document
SELECT JSON_PATH_QUERY_FIRST(payload, '$.tags[0]') FROM api_responses;

-- Check if a path exists
SELECT JSON_PATH_EXISTS(payload, '$.user.email') FROM api_responses;
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

## Array Types

Any scalar type can be declared as a 1-D or multi-dimensional array by appending
`[]` (or `ARRAY`) to the type name.

| Syntax          | Meaning                            |
|-----------------|------------------------------------|
| `INT[]`         | 1-D integer array                  |
| `TEXT[][]`      | 2-D text array                     |
| `BOOL[3]`       | 1-D boolean array (size hint only) |
| `ARRAY[INT]`    | keyword form for 1-D `INT[]`       |

```sql
CREATE TABLE tags_example (
    id   INT    PRIMARY KEY,
    tags TEXT[],
    nums INT[]
);

-- Array literals use ARRAY[e1, e2, ...]
INSERT INTO tags_example VALUES (1, ARRAY['alpha','beta'], ARRAY[10, 20, 30]);

-- 1-based subscript (PostgreSQL-compatible)
SELECT tags[1] FROM tags_example WHERE id = 1;   -- 'alpha'

-- Slice
SELECT nums[1:2] FROM tags_example WHERE id = 1; -- {10,20}

-- Containment: does array contain all elements of the right side?
SELECT nums @> ARRAY[20] FROM tags_example;       -- TRUE

-- Concatenation
SELECT ARRAY[1,2] || ARRAY[3,4];                  -- {1,2,3,4}
```

### Array Functions

| Function | Description |
|----------|-------------|
| `array_length(arr, dim)` | Length of dimension `dim` (1-based) |
| `cardinality(arr)` | Total number of elements across all dimensions |
| `array_ndims(arr)` | Number of dimensions |
| `array_append(arr, elem)` | Add element at end |
| `array_prepend(elem, arr)` | Add element at front |
| `array_cat(a, b)` | Concatenate two arrays (same as `\|\|`) |
| `array_remove(arr, val)` | Remove all occurrences of `val` |
| `array_replace(arr, old, new)` | Replace all occurrences of `old` with `new` |
| `array_upper(arr, dim)` / `array_lower(arr, dim)` | Upper/lower bound of dimension |
| `array_fill(val, dims)` | Create array filled with `val` |
| `array_to_string(arr, delim)` | Join elements with delimiter |
| `string_to_array(str, delim)` | Split string into array |
| `array_position(arr, elem)` | 1-based position of first occurrence (NULL if not found) |
| `array_positions(arr, elem)` | All positions of `elem` |
| `unnest(arr)` | Expand array into a set of rows |
| `array_agg(expr ORDER BY ...)` | Aggregate values into an array |

### ANY / ALL Operators

```sql
-- True if any element equals the left operand
SELECT 20 = ANY(ARRAY[10, 20, 30]);   -- TRUE

-- True if all elements satisfy the comparison
SELECT 5 < ALL(ARRAY[10, 20, 30]);    -- TRUE
```

### Array Operators

| Operator | Meaning |
|----------|---------|
| `@>` | Contains (left contains all elements of right) |
| `<@` | Contained by |
| `&&` | Overlaps (have any element in common) |
| `\|\|` | Concatenation |
| `=`, `<>` | Element-wise equality / inequality |

GIN indexes can be created on array columns to accelerate `@>` and `&&` queries:

```sql
CREATE INDEX ON tags_example USING GIN (tags);
-- Now "WHERE tags @> ARRAY['alpha']" uses the GIN index.
```

---

## Range Types

Range types represent a contiguous span of values with configurable lower and upper bounds.

| Type | Element type | Discrete canonicalization |
|------|-------------|--------------------------|
| `INT4RANGE` | `INT` | yes — `(a,b]` → `[a+1,b+1)` |
| `INT8RANGE` | `BIGINT` | yes |
| `DATERANGE` | `DATE` | yes |
| `NUMRANGE` | `DECIMAL` | no |
| `TSRANGE` | `TIMESTAMP` | no |

Bound notation follows PostgreSQL:
- `[` / `]` = inclusive bound
- `(` / `)` = exclusive bound
- empty string for a bound = unbounded (−∞ or +∞)
- `empty` = the empty range (contains no points)

```sql
-- Constructors: rangeType(lower, upper [, bounds])
SELECT int4range(1, 10);          -- [1,10)  (default bounds)
SELECT int4range(1, 5, '[]');     -- [1,6)   (inclusive → canonicalized)
SELECT int4range(1, 5, '()');     -- [2,5)   (exclusive lower → canonicalized)
SELECT int4range(5, 5);           -- empty

-- Cast from text literal
SELECT CAST('[1,5)' AS INT4RANGE);
SELECT 'empty'::INT4RANGE;

-- CREATE TABLE with range column
CREATE TABLE reservations (
    id   INT PRIMARY KEY,
    slot INT4RANGE
);
INSERT INTO reservations VALUES (1, int4range(9, 17));
SELECT slot FROM reservations;    -- [9,17)
```

### Range Operators

| Operator | Meaning | Example |
|----------|---------|---------|
| `@>` | Contains element or range | `int4range(1,10) @> 5` → `TRUE` |
| `<@` | Is contained by | `5 <@ int4range(1,10)` → `TRUE` |
| `&&` | Overlaps | `int4range(1,5) && int4range(4,8)` → `TRUE` |
| `+` | Union (adjacent or overlapping) | `int4range(1,5) + int4range(5,10)` → `[1,10)` |
| `*` | Intersection | `int4range(1,8) * int4range(4,12)` → `[4,8)` |
| `-` | Difference (non-interior) | `int4range(1,10) - int4range(1,5)` → `[5,10)` |
| `=`, `<>` | Equality / inequality | |
| `<`, `<=`, `>`, `>=` | Ordering by lower then upper bound | |

### Range Scalar Functions

| Function | Returns | Description |
|----------|---------|-------------|
| `lower(r)` | element or NULL | Lower bound; NULL if unbounded or empty |
| `upper(r)` | element or NULL | Upper bound; NULL if unbounded or empty |
| `isempty(r)` | BOOL | TRUE if the range is empty |
| `lowerinc(r)` | BOOL | TRUE if lower bound is inclusive |
| `upperinc(r)` | BOOL | TRUE if upper bound is inclusive |

```sql
-- Containment query: find slots that include hour 7
SELECT id FROM reservations WHERE slot @> 7;

-- Overlap query: find conflicting reservations
SELECT a.id, b.id
FROM reservations a, reservations b
WHERE a.id < b.id AND a.slot && b.slot;
```

<div class="callout-advantage">
**Advantage over application-level checks:** range operators evaluate containment and overlap in a single expression, eliminating the four-way `lo1 < hi2 AND hi1 > lo2` check that is easy to get wrong when mixing inclusive/exclusive bounds.
</div>

---

## LTREE — Hierarchical Path Type

`LTREE` stores a dot-separated label path where every label matches `[A-Za-z0-9_]+`.
It is designed for tree-structured data: org charts, file-system paths, category
hierarchies, DNS zones.

```sql
CREATE TABLE categories (id INT, path LTREE);

INSERT INTO categories VALUES
  (1, 'electronics'),
  (2, 'electronics.phones'),
  (3, 'electronics.phones.smartphones'),
  (4, 'electronics.laptops');

-- All descendants of electronics.phones
SELECT id FROM categories
WHERE 'electronics.phones'::LTREE @> path;
-- Returns ids 2 and 3
```

### LTREE Operators

| Operator | Meaning | Example |
|---|---|---|
| `@>` | Left is ancestor of (or equal to) right | `'a.b'::LTREE @> 'a.b.c'::LTREE` → true |
| `<@` | Left is descendant of (or equal to) right | `'a.b.c'::LTREE <@ 'a.b'::LTREE` → true |
| `~` | Left matches lquery pattern | `'a.b.c'::LTREE ~ 'a.*.c'` → true |
| `\|\|` | Concatenate two ltree paths | `'a.b'::LTREE \|\| 'c'::LTREE` → `'a.b.c'` |
| `=`, `<>` | Exact path equality | |
| `<`, `<=`, `>`, `>=` | Lexicographic path order | |

**lquery patterns**: Use `*` to match one or more labels at that position.
`'a.*.c'` matches any path that starts with `a`, ends with `c`, and has exactly
one label in between.

### LTREE Functions

| Function | Returns | Description |
|---|---|---|
| `nlevel(path)` | `INT` | Number of labels |
| `subpath(path, offset[, len])` | `LTREE` | Suffix starting at offset (negative = from end) |
| `subltree(path, start, end)` | `LTREE` | Labels `[start, end)` |
| `index(path, subpath[, offset])` | `INT` | First position of subpath (-1 if not found) |
| `lca(path, ...)` | `LTREE` | Longest common ancestor of all arguments |
| `text2ltree(text)` | `LTREE` | Parse and validate text as an ltree path |
| `ltree2text(path)` | `TEXT` | Extract the raw path string |

```sql
SELECT nlevel('a.b.c.d'::LTREE);          -- 4
SELECT subpath('a.b.c.d'::LTREE, 1, 2);   -- 'b.c'
SELECT subltree('a.b.c.d'::LTREE, 0, 2);  -- 'a.b'
SELECT index('a.b.c.a.b'::LTREE, 'a.b'::LTREE); -- 0
SELECT lca('a.b.c'::LTREE, 'a.b.d'::LTREE);     -- 'a.b'
SELECT text2ltree('org.eng');                     -- 'org.eng'::LTREE
SELECT ltree2text('org.eng'::LTREE);              -- 'org.eng'
```

### Cast to/from TEXT

```sql
SELECT 'org.eng.backend'::LTREE;           -- Ltree literal
SELECT CAST('org.eng.backend'::LTREE AS TEXT); -- text string
```

Invalid paths (empty labels, double dots, illegal characters) raise an error:

```sql
SELECT 'a..b'::LTREE;  -- Error: empty label
SELECT 'a b'::LTREE;   -- Error: space is not a valid label character
```

<div class="callout-advantage">
<strong>AxiomDB vs. TEXT columns for hierarchies.</strong>
Without <code>LTREE</code> the common workaround is to store a materialized path in a
<code>TEXT</code> column and use <code>LIKE 'org.eng%'</code>. That requires a leading-
wildcard safe index and breaks the moment a label contains a dot. <code>LTREE</code>
validates the label grammar, provides semantically correct ancestor/descendant operators,
and exposes the <code>lca</code> and <code>subpath</code> functions that are impossible
to replicate with plain strings.
</div>

---

## XML / XMLTYPE — Document Type (Phase 20.20)

`XML` (alias `XMLTYPE`) stores a UTF-8 XML text value. The stored string must be
well-formed XML (either a complete document or a valid XML fragment). Validation
is performed on write.

```sql
CREATE TABLE docs (id INT, content XML);

INSERT INTO docs VALUES (1, '<root><item id="1">hello</item></root>');

SELECT content FROM docs WHERE id = 1;
-- Returns: <root><item id="1">hello</item></root>
```

### XML Coercions

```sql
-- Cast a text literal to XML (validates well-formedness)
SELECT CAST('<root/>' AS XML);
SELECT '<root/>'::XML;

-- Cast XML back to TEXT
SELECT CAST('<root/>'::XML AS TEXT);

-- Invalid XML raises an error
SELECT CAST('<broken' AS XML);  -- Error: InvalidCoercion
```

### XML Functions

| Function | Returns | Description |
|---|---|---|
| `xml_is_well_formed(text)` | `INT` | 1 if the string is valid XML, 0 if not, NULL if input is NULL |

```sql
SELECT xml_is_well_formed('<a/>');           -- 1
SELECT xml_is_well_formed('<?xml version="1.0"?><root/>'); -- 1
SELECT xml_is_well_formed('<broken');        -- 0
SELECT xml_is_well_formed(NULL);             -- NULL
```

### XML Constructor Functions

AxiomDB implements the SQL/XML constructor functions defined in SQL:2006.

| Form | Returns | Description |
|---|---|---|
| `XMLELEMENT(NAME tag [, XMLATTRIBUTES(v AS a, ...) ] [, content ...])` | `XML` | Build an element with optional attributes and content |
| `XMLFOREST(expr AS name [, ...])` | `XML` | Build a sequence of sibling elements |
| `XMLROOT(xml_expr, VERSION str [, STANDALONE YES\|NO])` | `XML` | Wrap XML with an XML declaration |
| `XMLCONCAT(xml1, ...)` | `XML` | Concatenate XML fragments (NULLs skipped) |
| `XMLQUERY(xpath PASSING xml_expr [RETURNING CONTENT])` | `TEXT` | Evaluate a minimal XPath expression |

```sql
-- Build an element
SELECT XMLELEMENT(NAME person,
    XMLATTRIBUTES(42 AS id),
    XMLFOREST('Alice' AS name, 30 AS age));
-- <person id="42"><name>Alice</name><age>30</age></person>

-- Wrap with XML declaration
SELECT XMLROOT('<root/>'::XML, VERSION '1.0', STANDALONE YES);
-- <?xml version="1.0" standalone="yes"?><root/>

-- Concatenate fragments
SELECT XMLCONCAT('<a/>'::XML, '<b/>'::XML);
-- <a/><b/>

-- XPath extraction
SELECT XMLQUERY('/root/item/text()' PASSING '<root><item>hello</item></root>'::XML);
-- 'hello'

-- Attribute extraction
SELECT XMLQUERY('/doc/elem/@id' PASSING '<doc><elem id="99"/></doc>'::XML);
-- '99'
```

**XPath support**: `XMLQUERY` supports absolute paths (`/elem/...`) with `text()` and `@attr` terminal steps. Element names without a terminal step return the element's text content.

### XMLTABLE — Shred XML into Rows

`XMLTABLE` is a table-valued function (TVF) that turns an XML document into a set of rows. It appears in the `FROM` clause.

```sql
SELECT col1, col2
FROM XMLTABLE(
    'row_xpath'           -- XPath that selects one node per output row
    PASSING xml_expr      -- expression that produces the XML document
    COLUMNS
        col1  TYPE [PATH 'xpath'] [DEFAULT expr],
        col2  TYPE [PATH 'xpath'],
        ord   FOR ORDINALITY     -- 1-based row counter
) AS t;
```

**Column PATH**: XPath relative to the row node. Defaults to the column name as a child element path. Use `@attr` for attribute values.

**FOR ORDINALITY**: Integer column auto-assigned 1, 2, 3, … for each row in document order.

**DEFAULT**: Value used when the PATH matches no node (instead of NULL).

**NULL propagation**: If the PASSING expression evaluates to NULL or unparseable text, XMLTABLE returns zero rows.

```sql
-- Basic shredding
SELECT name, age
FROM XMLTABLE(
    '/rows/row'
    PASSING '<rows><row><name>Alice</name><age>30</age></row></rows>'
    COLUMNS name TEXT, age INT
) AS t;
-- name='Alice', age=30

-- Attribute extraction
SELECT id, val
FROM XMLTABLE(
    '/items/item'
    PASSING '<items><item id="1">hello</item></items>'
    COLUMNS id INT PATH '@id', val TEXT
) AS t;
-- id=1, val='hello'

-- Ordinality + custom path + default
SELECT ord, label
FROM XMLTABLE(
    '/data/entry'
    PASSING '<data><entry/><entry><lbl>x</lbl></entry></data>'
    COLUMNS ord FOR ORDINALITY, label TEXT PATH 'lbl' DEFAULT 'n/a'
) AS t;
-- row 1: ord=1, label='n/a'
-- row 2: ord=2, label='x'
```

<div class="callout-advantage">
<strong>AxiomDB advantage vs MySQL</strong>: MySQL has no XMLTABLE — XML shredding requires application-side parsing. AxiomDB's XMLTABLE follows the SQL:2006 / PostgreSQL 10+ standard and supports full <code>WHERE</code>, <code>ORDER BY</code>, <code>JOIN</code>, and aggregation against the derived table.
</div>

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
