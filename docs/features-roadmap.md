# AxiomDB — Features Roadmap (Wishlist)

Fases movidas de `progreso.md` porque **no son estrictamente necesarias** para tener una base de datos real funcional. Son extensiones / features producto que se evaluarán después de estabilizar el core DB.

Criterio: una "DB real" necesita storage + WAL + MVCC + SQL + índices + joins + types + transactions + security + HA + observabilidad + backup. Lo que está aquí es **valor-agregado** sobre ese core:

- Sintaxis alternativas (AxiomQL, GraphQL, OData)
- Emulación de otros sistemas (MongoDB CDC, DoltDB Git-for-data, Arrow Flight)
- Verticales específicas (Vector/AI, GIS)
- Deployment alternativo (Browser Wasm)
- Escalado distribuido (sharding, Raft, 2PC)

Cuando el core DB esté estable, extraer una fase de aquí, renumerar, mover de vuelta a `progreso.md`.

---

### Phase 15 — MongoDB + DoltDB + Arrow `⏳` week 34-35
- [ ] 15.1 ⏳ Change streams CDC — tail the WAL, emit Insert/Update/Delete events
- [ ] 15.2 ⏳ Git for data — commits, branches, checkout with snapshot of roots
- [ ] 15.3 ⏳ Git merge — branch merge with conflict detection
- [ ] 15.4 ⏳ Apache Arrow output — results in columnar format for Python/pandas
- [ ] 15.5 ⏳ Flight SQL — Arrow Flight protocol for high-speed columnar transfer (Python, Rust, Java without JDBC)
- [ ] 15.6 ⏳ CDC + Git tests — verify change streams and branch merge with real conflicts
- [ ] 15.7 ⏳ CDC with full OLD/NEW row — `REPLICA IDENTITY FULL` equivalent;
- [ ] 15.8 ⏳ Flashback Table — `FLASHBACK TABLE empleados TO TIMESTAMP NOW() - INTERVAL '2 hours'` restores the table to its state at that point in time using WAL history; different from Phase 7.16 AS OF (which is read-only): Flashback Table actually replaces current data with historical data; `FLASHBACK TABLE pedidos TO SCN 1234567` using the WAL sequence number for precision; requires retaining enough WAL history (configurable retention window); use case: "I accidentally ran UPDATE without WHERE on production — restore the table to 5 minutes ago"; extends Phase 15.2 (Git for data) to a SQL-native restore operation; Oracle Flashback Technology (2003) is still unique in databases — no PostgreSQL or MySQL equivalent exists UPDATE events include the complete before-image (all column values before the change) and after-image; without this, UPDATE events in CDC only show the new values and primary key, making it impossible to detect which specific fields changed; required for audit trails, sync systems, and data pipelines that need to compute diffs

---

### Phase 22 — Vector search + advanced search + GIS `⏳` week 52-54
- [ ] 22.1 ⏳ Vector similarity — `VECTOR(n)`, operators `<=>`, `<->`, `<#>`
- [ ] 22.2 ⏳ HNSW index — `CREATE INDEX USING hnsw(col vector_cosine_ops)`
- [ ] 22.3 ⏳ Fuzzy search — `SIMILARITY()`, trigrams, `LEVENSHTEIN()`
- [ ] 22.4 ⏳ ANN benchmarks — compare HNSW vs pgvector vs FAISS on recall@10 and QPS; document quality/speed tradeoff
- [ ] 22.5 ⏳ IVFFlat alternative index — lower RAM option than HNSW for collections >10M vectors
- [ ] 22.6 ⏳ GIS: Spatial data types — POINT, LINESTRING, POLYGON, MULTIPOINT, MULTIPOLYGON, GEOMETRY; stored compactly as WKB (Well-Known Binary); implements axiomdb-geo crate (currently stub); required by every delivery, store-locator, logistics, real-estate, and fleet-management application
- [ ] 22.7 ⏳ GIS: R-Tree spatial index — `CREATE INDEX ON locations USING rtree(coords)`; O(log n) bounding box queries; without this every spatial query is a full table scan; enables `WHERE ST_DWithin(location, point, 5000)` in milliseconds over millions of points
- [ ] 22.8 ⏳ GIS: Core spatial functions — `ST_Distance`, `ST_Within`, `ST_Contains`, `ST_Intersects`, `ST_Area`, `ST_Length`, `ST_Buffer`, `ST_Union`, `ST_AsText`, `ST_GeomFromText`; the minimum vocabulary for geographic queries; `SELECT * FROM stores WHERE ST_Distance(location, ST_Point(-74.0, 40.7)) < 5000`
- [ ] 22.9 ⏳ GIS: Coordinate system support — WGS84 (GPS coordinates) and local projections; `ST_Transform(geom, 4326)` converts between SRID systems; without this distances are in degrees instead of meters
- [ ] 22.10 ⏳ GIS: Spatial benchmarks — compare range query and nearest-neighbor vs PostGIS on 1M point dataset; document performance characteristics
- [ ] 22.11 ⏳ Approximate query processing — `SELECT APPROX_COUNT_DISTINCT(user_id) FROM events` uses HyperLogLog (error < 2%, 10000x faster than COUNT DISTINCT); `SELECT PERCENTILE_APPROX(response_ms, 0.95) FROM requests` uses t-digest (accurate tail estimation); `SELECT APPROX_TOP_K(product_id, 10) FROM purchases` returns approximate top-10 using Count-Min Sketch; for analytics on billions of rows where exact answers take minutes and approximate answers (99.9% accurate) take milliseconds

### Phase 22c — Native GraphQL API `⏳` week 58-60
- [ ] 22c.1 ⏳ GraphQL server on port `:3308` — schema auto-discovered from catalog
- [ ] 22c.2 ⏳ GraphQL queries and mutations — mapped to point lookups and range scans on B+ Tree
- [ ] 22c.3 ⏳ GraphQL subscriptions — WAL as event stream, WebSocket, no polling
- [ ] 22c.4 ⏳ GraphQL DataLoader — automatic batch loading, eliminates N+1 problem
- [ ] 22c.5 ⏳ GraphQL introspection — full schema for Apollo Studio, Postman, codegen
- [ ] 22c.6 ⏳ GraphQL persisted queries — pre-registered query hash; avoids transmitting the full document in production
- [ ] 22c.7 ⏳ GraphQL end-to-end tests — queries, mutations, subscriptions with real client (gqlgen/graphql-request)

### Phase 22d — Native OData v4 `⏳` week 61-63
- [ ] 22d.1 ⏳ HTTP endpoint `:3309` — compatible with PowerBI, Excel, Tableau, SAP without drivers
- [ ] 22d.2 ⏳ OData `$metadata` — EDMX document auto-discovered from catalog (PowerBI consumes it on connect)
- [ ] 22d.3 ⏳ OData queries — `$filter`, `$select`, `$orderby`, `$top`, `$skip`, `$count` mapped to SQL
- [ ] 22d.4 ⏳ OData `$expand` — JOINs by FK: `/odata/orders?$expand=customer` without manual SQL
- [ ] 22d.5 ⏳ OData batch requests — multiple operations in a single HTTP request (`$batch`)
- [ ] 22d.6 ⏳ OData authentication — Bearer token + Basic Auth for enterprise connectors
- [ ] 22d.7 ⏳ OData end-to-end tests — connect real Excel/PowerBI + automated $filter/$expand/$batch suite

### Phase 22e — Native Toolkit System `⏳` week 64-67

> **Design:** `db.md` § "Native Toolkit System" — the complete spec.
> Toolkits are built-in domain packs (blog, ecommerce, iot, saas, analytics) that activate
> types, functions, schema templates, optimizer hints, and monitoring views with one SQL command.
> Zero external dependencies — everything compiled into the binary.

#### 22e.A — Core infrastructure
- [ ] 22e.1 ⏳ `INSTALL TOOLKIT` / `UNINSTALL TOOLKIT` / `LIST TOOLKITS` — DDL parser + executor; persists activation in `axiom_toolkits` catalog table; one row per installed toolkit with name, version, installed_at
- [ ] 22e.2 ⏳ `DESCRIBE TOOLKIT name` — shows types, functions, templates, and monitoring views provided by the toolkit
- [ ] 22e.3 ⏳ `axiom_toolkits` system view — name, version, installed_at, objects_count
- [ ] 22e.4 ⏳ `axiom_toolkit_objects` system view — object_type, object_name, schema, toolkit
- [ ] 22e.5 ⏳ `axiom_toolkit_functions` system view — function_name, signature, toolkit, description
- [ ] 22e.6 ⏳ Schema templates — `CREATE TABLE t LIKE TOOLKIT blog.posts`; generates DDL with best-practice column definitions, constraints, indexes, and RLS policies for the template; does NOT auto-create tables
- [ ] 22e.7 ⏳ Toolkit optimizer hints — planner reads `axiom_toolkits` at session start; adjusts prefetch strategy, join preference, and index suggestion thresholds based on declared workload (read-heavy/write-heavy/analytical)
- [ ] 22e.8 ⏳ Toolkit combinability — multiple toolkits can be installed simultaneously; their namespaces are orthogonal (`toolkit_blog.*`, `toolkit_saas.*`); conflict detection for overlapping type names

#### 22e.B — Toolkit: blog
- [ ] 22e.10 ⏳ Domain types — `SLUG TEXT CHECK (value ~ '^[a-z0-9][a-z0-9-]*[a-z0-9]$')`, `POST_STATUS ENUM('draft','published','scheduled','archived')`, `READING_LEVEL ENUM('easy','moderate','advanced')`
- [ ] 22e.11 ⏳ Domain functions — `SLUG(text)→TEXT` (normalizes to URL-safe slug), `EXCERPT(text, max_words INT)→TEXT`, `READING_TIME(text)→INT` (minutes at 200 wpm), `WORD_COUNT(text)→INT`, `EXTRACT_HEADINGS(text)→TEXT[]`, `RANK_POSTS(query TEXT, col TEXT)→REAL` (BM25 + recency score)
- [ ] 22e.12 ⏳ Schema templates — `blog.posts` (id, title, slug SLUG, content, excerpt, author_id, status POST_STATUS, published_at, fts_vector; + partial index on published_at WHERE status='published', FTS index), `blog.comments` (with parent_id for nesting), `blog.tags`, `blog.post_tags`, `blog.categories` (with ltree path)
- [ ] 22e.13 ⏳ Monitoring — `axiom_blog_stats` (post_count by status, draft_count, avg_reading_time, comment_count_today, top_tags TEXT[])

#### 22e.C — Toolkit: ecommerce
- [ ] 22e.20 ⏳ Domain types — `MONEY` composite `(amount DECIMAL(12,4), currency CHAR(3))` with `+`, `-`, `*` operators, `SKU TEXT CHECK (value ~ '^[A-Z0-9][A-Z0-9\-_]{1,63}$')`, `ORDER_STATUS ENUM('pending','confirmed','processing','shipped','delivered','cancelled','refunded')`
- [ ] 22e.21 ⏳ Domain functions — `APPLY_TAX(amount, country CHAR(2), category TEXT)→MONEY`, `CONVERT_CURRENCY(amount DECIMAL, from CHAR(3), to CHAR(3))→DECIMAL` (uses `axiom_exchange_rates`), `NEXT_INVOICE_NUM(series TEXT)→TEXT` (gapless sequence, same guarantee as 13.10)
- [ ] 22e.22 ⏳ Inventory functions — `RESERVE_INVENTORY(sku, qty INT, session_id TEXT)→BIGINT` (returns reservation_id), `COMMIT_RESERVATION(reservation_id BIGINT)→BOOL`, `RELEASE_RESERVATION(reservation_id BIGINT)→BOOL`; reservations stored in `toolkit_ecommerce.reservations` with TTL
- [ ] 22e.23 ⏳ Schema templates — `ecommerce.products`, `ecommerce.inventory` (sku, stock, reserved, available as generated column), `ecommerce.orders`, `ecommerce.order_items`, `ecommerce.invoices` (gapless seq, fiscal period aware)
- [ ] 22e.24 ⏳ Monitoring — `axiom_inventory_status` (sku, stock, reserved, available), `axiom_order_pipeline` (orders by status + age bucket), `axiom_revenue_today` (total by currency)

#### 22e.D — Toolkit: iot
- [ ] 22e.30 ⏳ Domain types — `DEVICE_STATUS ENUM('active','inactive','error','maintenance')`, `READING_QUALITY ENUM('good','uncertain','bad')`
- [ ] 22e.31 ⏳ Domain functions — `TIME_BUCKET(bucket INTERVAL, ts TIMESTAMP)→TIMESTAMP` (like TimescaleDB), `DEAD_BAND(new_val REAL, prev_val REAL, threshold REAL)→BOOL`, `INTERPOLATE_LOCF(ts TIMESTAMP, val REAL)→REAL`, `INTERPOLATE_LINEAR(ts1 TIMESTAMP, v1 REAL, ts2 TIMESTAMP, v2 REAL, target TIMESTAMP)→REAL`, `SENSOR_DRIFT(readings REAL[], expected REAL)→REAL`
- [ ] 22e.32 ⏳ Schema templates — `iot.devices` (id, name, type, location POINT, status), `iot.readings` (device_id, ts, value, quality; auto-partitioned by month, BRIN on ts, TTL configurable), `iot.alerts` (device_id, ts, severity, message, resolved_at)
- [ ] 22e.33 ⏳ Monitoring — `axiom_device_status` (last_seen, reading_count_24h, alert_count_open per device), `axiom_data_freshness` (table, last_insert, expected_interval, status), `axiom_sensor_health` (devices silent for > expected interval)

#### 22e.E — Toolkit: saas
- [ ] 22e.40 ⏳ Domain types — `TENANT_ID BIGINT NOT NULL`, `SUBSCRIPTION_TIER ENUM('free','starter','pro','enterprise')`
- [ ] 22e.41 ⏳ Domain functions — `CURRENT_TENANT()→BIGINT` (reads from session variable `app.tenant_id`), `TENANT_QUOTA_CHECK(resource TEXT, amount BIGINT)→BOOL` (consults `axiom_quota_limits`), `ANONYMIZE(text TEXT)→TEXT` (SHA-256 prefix, GDPR-safe), `MASK_PII(text TEXT, policy TEXT)→TEXT`
- [ ] 22e.42 ⏳ Auto-RLS — when saas toolkit is active, `CREATE TABLE` with a `tenant_id` column automatically gets a RLS policy `USING (tenant_id = CURRENT_TENANT())`; opt-out via `WITH (no_toolkit_rls = true)`
- [ ] 22e.43 ⏳ Schema templates — `saas.tenants`, `saas.subscriptions`, `saas.audit_log` (immutable, append-only via 13.9), `saas.quota_usage`
- [ ] 22e.44 ⏳ Monitoring — `axiom_tenant_usage` (tenant_id, storage_bytes, row_count, queries_today), `axiom_quota_alerts` (tenants at >80% of any quota), `axiom_compliance_log` (accesses to PII columns with user + timestamp)

#### 22e.F — Toolkit: analytics
- [ ] 22e.50 ⏳ Domain functions — `PERCENTILE_RANK(value REAL, dataset REAL[])→REAL`, `Z_SCORE(value REAL, mean REAL, stddev REAL)→REAL`, `MOVING_AVG(col, window_size INT)→REAL` (sugar for window function), `COHORT_DATE(ts TIMESTAMP, granularity TEXT)→DATE` ('week'/'month'/'quarter'), `RETENTION_RATE(cohort_date DATE, event_date DATE)→REAL`, `FUNNEL_STEP(user_id BIGINT, step INT, ts TIMESTAMP)→BOOL`
- [ ] 22e.51 ⏳ Schema templates — `analytics.events` (user_id, event TEXT, ts, properties JSON; GIN on properties), `analytics.sessions` (session_id, user_id, started_at, ended_at, event_count), `analytics.funnels` (funnel_id, step_order, event_name, description)
- [ ] 22e.52 ⏳ Monitoring — `axiom_query_stats` (top queries by cost + frequency), `axiom_slow_analytical` (analytical queries > threshold), `axiom_cache_efficiency` (buffer pool hit rate per table)

#### 22e.G — Quality
- [ ] 22e.60 ⏳ Toolkit combination tests — install blog+saas, ecommerce+saas, iot+analytics; verify no namespace conflicts, RLS applies correctly, optimizer hints don't conflict
- [ ] 22e.61 ⏳ Schema template tests — `CREATE TABLE LIKE TOOLKIT x.y`; verify generated DDL compiles, indexes are created, RLS policies are attached
- [ ] 22e.62 ⏳ Domain function tests — unit tests for every toolkit function; edge cases (empty string, NULL, overflow, invalid currency code)
- [ ] 22e.63 ⏳ Monitoring view tests — insert test data, verify all `axiom_*` views return correct aggregates
- [ ] 22e.64 ⏳ Documentation — user guide page per toolkit: SQL examples, schema template output, monitoring queries, combination guide

---

### Phase 33 — AI embeddings + hybrid search `⏳` week 94-99
- [ ] 33.1 ⏳ AI_EMBED() — local Ollama (primary) + OpenAI (fallback) + cache
- [ ] 33.2 ⏳ VECTOR GENERATED ALWAYS AS (AI_EMBED(col)) STORED
- [ ] 33.3 ⏳ Hybrid search — BM25 + HNSW + RRF in a single query
- [ ] 33.4 ⏳ Re-ranking — cross-encoder for more accurate results

### Phase 33b — AI functions `⏳` week 100-101
- [ ] 33b.1 ⏳ AI_CLASSIFY(), AI_EXTRACT(), AI_SUMMARIZE(), AI_TRANSLATE()
- [ ] 33b.2 ⏳ AI_DETECT_PII() + AI_MASK_PII() — automatic privacy
- [ ] 33b.3 ⏳ AI function tests — deterministic mocks of Ollama/OpenAI for CI; verify latency and fallback
- [ ] 33b.4 ⏳ AI function rate limiting — throttle calls to the external model; token budget per role/session

### Phase 33c — RAG + Model Store `⏳` week 102-103
- [ ] 33c.1 ⏳ RAG Pipeline — `CREATE RAG PIPELINE` + `RAG_QUERY()`
- [ ] 33c.2 ⏳ Feature Store — `CREATE FEATURE GROUP` + point-in-time correct
- [ ] 33c.3 ⏳ Model Store ONNX — `CREATE MODEL` + `PREDICT()` + `PREDICT_AB()`
- [ ] 33c.4 ⏳ RAG evaluation — precision/recall metrics of RAG pipeline; compare with BM25 search baseline

### Phase 33d — AI intelligence + privacy `⏳` week 104-106
- [ ] 33d.1 ⏳ Adaptive indexing — automatic index suggestions based on query history
- [ ] 33d.2 ⏳ Text-to-SQL — `NL_QUERY()`, `NL_TO_SQL()`, `NL_EXPLAIN()`
- [ ] 33d.3 ⏳ Anomaly detection — `ANOMALY_SCORE()` + `CREATE ANOMALY DETECTOR`
- [ ] 33d.4 ⏳ Differential privacy — `DP_COUNT`, `DP_AVG` with budget per role
- [ ] 33d.5 ⏳ Data lineage — `DATA_LINEAGE()` + GDPR Right to be Forgotten

### Phase 34 — Distributed infrastructure `⏳` week 107-110
- [ ] 34.1 ⏳ Sharding — `DISTRIBUTED BY HASH/RANGE/LIST` across N nodes
- [ ] 34.2 ⏳ Scatter-gather — execute plan on shards in parallel + merge
- [ ] 34.3 ⏳ Shard rebalancing — without downtime
- [ ] 34.4 ⏳ Logical decoding API — `pg_logical_slot_get_changes()` as JSON
- [ ] 34.5 ⏳ Standard DSN — `axiomdb://`, `postgres://`, `DATABASE_URL` env var
- [ ] 34.6 ⏳ Extensions system — `CREATE EXTENSION` + `pg_available_extensions`
- [ ] 34.7 ⏳ WASM extensions — `CREATE EXTENSION FROM FILE '*.wasm'`
- [ ] 34.8 ⏳ VACUUM FREEZE — prevent Transaction ID Wraparound
- [ ] 34.9 ⏳ Parallel DDL — `CREATE TABLE AS SELECT WITH PARALLEL N`
- [ ] 34.10 ⏳ pgbench equivalent — `axiomdb-bench` with standard OLTP scenarios
- [ ] 34.11 ⏳ Final benchmarks — full comparison vs MySQL, PostgreSQL, SQLite, DuckDB
- [ ] 34.12 ⏳ Consensus protocol (basic Raft) — for automatic failover in cluster; replaces manual failover from 18.10
- [ ] 34.13 ⏳ Distributed transactions — two-phase commit between shards; cross-shard consistency

### Phase 36 — AxiomQL Core (SELECT + READ) `⏳` week 114-117

#### 36.A — Foundation
- [ ] 36.1 ⏳ AxiomQL lexer — `.`, `(`, `)`, `:` named args, operators, string/number/bool literals, identifiers, `@` decorators
- [ ] 36.2 ⏳ Core SELECT: `.filter()`, `.sort()`, `.take()`, `.pick()`, `.skip()` → compile to SQL `Stmt`
- [ ] 36.3 ⏳ `.distinct()` — removes duplicate rows; `.distinct(col)` = DISTINCT ON(col)

#### 36.B — Joins
- [ ] 36.4 ⏳ `.join(table)` — auto-infers ON from FK catalog; `.join(orders, on: user_id)` for explicit
- [ ] 36.5 ⏳ `.left_join()`, `.right_join()`, `.full_join()`, `.cross_join()` — all join types
- [ ] 36.6 ⏳ `.join(table.join(other))` — nested/chained joins for multi-table queries

#### 36.C — Aggregation
- [ ] 36.7 ⏳ `.group(col, agg: fn())` — GROUP BY with aggregates; no need to repeat group key in pick
- [ ] 36.8 ⏳ Aggregate functions: `count()`, `sum(col)`, `avg(col)`, `min(col)`, `max(col)`, `string_agg(col, sep)`
- [ ] 36.9 ⏳ Aggregate with filter: `count(where: active)`, `sum(amount, where: status = 'ok')` → compiles to AGG FILTER(WHERE)
- [ ] 36.10 ⏳ `.rollup(a, b)`, `.cube(a, b)`, `.grouping_sets([a], [b], [])` — analytical grouping
- [ ] 36.11 ⏳ Terminal aggregates: `users.count()`, `orders.sum(amount)`, `orders.avg(amount)` — no group needed

#### 36.D — Window functions
- [ ] 36.12 ⏳ `.window(col: fn().over(partition).sort(order))` — OVER clause; `row_number()`, `rank()`, `dense_rank()`
- [ ] 36.13 ⏳ Offset window functions: `lag(col)`, `lead(col)`, `first_value(col)`, `last_value(col)`, `nth_value(col, n)`
- [ ] 36.14 ⏳ Window aggregates: `sum(col).over(partition)`, `avg(col).over(partition).rows(preceding: 3)`
- [ ] 36.15 ⏳ Frame clauses: `.rows(unbounded_preceding)`, `.range(current_row)`, `.groups(n)` as chained methods

#### 36.E — Set operations + advanced subqueries
- [ ] 36.16 ⏳ `.union(other)`, `.union_all(other)`, `.intersect(other)`, `.except(other)` — set operations
- [ ] 36.17 ⏳ Subquery in `.filter()`: `users.filter(id in orders.filter(amount > 1000).pick(user_id))`
- [ ] 36.18 ⏳ `.exists(subquery)`, `.not_exists(subquery)` — EXISTS / NOT EXISTS
- [ ] 36.19 ⏳ Correlated subquery in `.pick()`: `users.pick(name, total: orders.filter(user_id = .id).sum(amount))`
- [ ] 36.20 ⏳ `let` bindings / named CTEs: `let top = orders.group(...)` → WITH clause; multiple lets compose
- [ ] 36.21 ⏳ Recursive CTE: `let tree = nodes.recursive(parent_id = .id)` → WITH RECURSIVE

#### 36.F — Expressions
- [ ] 36.22 ⏳ `match {}` — alternative to CASE WHEN: `match(status) { 'ok' → 1, _ → 0 }`
- [ ] 36.23 ⏳ Null-safe: `.filter(col.is_null())`, `.filter(col.not_null())`, `col.or(default)` → COALESCE
- [ ] 36.24 ⏳ JSON navigation: `data.name`, `data['key']`, `data.tags[0]` → JSON operators `->>` / `->` / `#>>`
- [ ] 36.25 ⏳ Full-text search: `.search(col, 'term')`, `.search(col, 'term', lang: 'english')` → tsvector/tsquery
- [ ] 36.26 ⏳ `.filter(col ~ 'regex')` — regex match operator

#### 36.G — Introspection + diagnostics
- [ ] 36.27 ⏳ `.explain()` — appends EXPLAIN; `.explain(analyze: true)` → EXPLAIN ANALYZE
- [ ] 36.28 ⏳ `show tables`, `show columns(users)`, `describe(users)` — introspection commands

#### 36.H — Advanced joins + inline data
- [ ] 36.32 ⏳ `.lateral_join(fn)` — LATERAL JOIN; fn receives outer row: `orders.lateral_join(o => items.filter(order_id = o.id).limit(3))`
- [ ] 36.33 ⏳ `values([[1,'a'],[2,'b']]).as('t', cols: [id, name])` — VALUES as inline table; useful in JOINs and CTEs
- [ ] 36.34 ⏳ `users.sample(pct: 10)` / `users.sample(rows: 1000)` — TABLESAMPLE SYSTEM; approximate random sample

#### 36.I — Statistical + ordered-set aggregates
- [ ] 36.35 ⏳ `orders.percentile(amount, 0.95)` → PERCENTILE_CONT(0.95) WITHIN GROUP (ORDER BY amount)
- [ ] 36.36 ⏳ `orders.percentile_disc(amount, 0.5)`, `orders.mode(status)` → PERCENTILE_DISC / MODE()
- [ ] 36.37 ⏳ `json_agg(expr)`, `json_build_object(k, v)`, `array_agg(col)` as aggregate functions in `.group()` and `.pick()`
- [ ] 36.38 ⏳ `table.unnest(col)` — UNNEST array column into rows

#### 36.J — Date/time + ranges
- [ ] 36.39 ⏳ `col.in_tz('America/Bogota')` → AT TIME ZONE; `col.format('YYYY-MM-DD')` → TO_CHAR
- [ ] 36.40 ⏳ Interval arithmetic: `created_at + interval(days: 7)`, `now() - interval(hours: 1)`
- [ ] 36.41 ⏳ `series(from: 1, to: 100)` / `series(from: date1, to: date2, step: interval(days: 1))` → GENERATE_SERIES
- [ ] 36.42 ⏳ Range operators: `period.overlaps(other)`, `period.contains(point)`, `period.adjacent(other)` → `&&`, `@>`, `-|-`

#### 36.K — Collation
- [ ] 36.43 ⏳ `.sort(name.collate('utf8mb4_unicode_ci'))` — per-expression COLLATE; `.filter(a.collate('C') = b)` for byte-level comparison

#### 36.L — Quality
- [ ] 36.44 ⏳ Equivalence test suite — for every AxiomQL construct, assert SQL equivalent produces identical results
- [ ] 36.45 ⏳ Parser benchmarks — AxiomQL throughput vs SQL parser on same queries
- [ ] 36.46 ⏳ Error messages — when a construct isn't supported: "use the SQL equivalent: SELECT ... OVER (...)"

### Phase 37 — AxiomQL Write + DDL + Control `⏳` week 118-121

#### 37.A — DML write
- [ ] 37.1 ⏳ `.insert(col: val, ...)` — single row; `users.insert_many([...])` — batch
- [ ] 37.2 ⏳ `.insert_select(query)` — INSERT INTO ... SELECT
- [ ] 37.3 ⏳ `.update(col: val, ...)` — UPDATE with filter chain
- [ ] 37.4 ⏳ `.delete()` — DELETE with filter chain
- [ ] 37.5 ⏳ `.upsert(on: col)` — INSERT ON CONFLICT DO UPDATE
- [ ] 37.6 ⏳ `.returning(col, ...)` — RETURNING clause on insert/update/delete; returns affected rows
- [ ] 37.7 ⏳ `.for_update()`, `.for_share()`, `.skip_locked()` — pessimistic locking on SELECT

#### 37.B — DDL
- [ ] 37.8 ⏳ `create table {}` with `@` decorators: `@primary`, `@auto`, `@unique`, `@required`, `@default(val)`, `@references(other.col)`
- [ ] 37.9 ⏳ `alter table` — `.add(col: type)`, `.drop(col)`, `.rename(old, new)`, `.rename_to(name)`
- [ ] 37.10 ⏳ `drop table`, `truncate table` — destructive DDL
- [ ] 37.11 ⏳ `create table_as(query)` — CREATE TABLE AS SELECT
- [ ] 37.12 ⏳ Indexes: `index table.col`, `index table(a, b)`, `@fulltext`, `@partial(filter_expr)`
- [ ] 37.13 ⏳ `migration 'name' { }` block — versioned schema changes with up/down

#### 37.C — Transactions + control flow
- [ ] 37.14 ⏳ `transaction { }` block — BEGIN/COMMIT with auto ROLLBACK on error
- [ ] 37.15 ⏳ `transaction(isolation: serializable) { }` — SET TRANSACTION ISOLATION LEVEL
- [ ] 37.16 ⏳ `savepoint 'name'` / `rollback to 'name'` / `release 'name'` inside transaction blocks
- [ ] 37.17 ⏳ `abort(msg)` inside transaction — manual ROLLBACK with error message

#### 37.D — Reusable logic
- [ ] 37.18 ⏳ `proc name(args) { }` — stored procedures in AxiomQL syntax
- [ ] 37.19 ⏳ `fn name(args) -> type { }` — user-defined functions; callable inside `.filter()`, `.pick()`
- [ ] 37.20 ⏳ `on table.after.insert { }`, `on table.before.update { }` — triggers with `.new` / `.old` access

#### 37.E — Temporal (requires Phase 7 MVCC time-travel)
- [ ] 37.21 ⏳ `users.as_of('2026-01-01')` — historical snapshot read → AS OF TIMESTAMP
- [ ] 37.22 ⏳ `users.history()` — all versions of rows → temporal scan
- [ ] 37.23 ⏳ `users.changes(from: t1, to: t2)` — delta between two snapshots

#### 37.G — Bulk I/O (COPY)
- [ ] 37.27 ⏳ `users.export('/path/file.csv', format: csv)` — COPY TO; also `format: json`, `format: parquet`
- [ ] 37.28 ⏳ `users.import('/path/file.csv', format: csv)` — COPY FROM with schema validation and error reporting
- [ ] 37.29 ⏳ `users.filter(...).export(query)` — export result of arbitrary query, not just full table

#### 37.H — Reactive queries (LISTEN/NOTIFY)
- [ ] 37.30 ⏳ `channel('name').listen()` — LISTEN channel; returns async stream of notifications
- [ ] 37.31 ⏳ `channel('name').notify(payload)` — NOTIFY channel, 'payload'
- [ ] 37.32 ⏳ `users.subscribe(filter: active)` — reactive query stream; uses WAL CatalogChangeNotifier from Phase 3.13

#### 37.I — Cursors (server-side iteration)
- [ ] 37.33 ⏳ `users.filter(...).cursor()` — server-side cursor for large result sets; compiles to DECLARE + CURSOR
- [ ] 37.34 ⏳ `.fetch(n)` / `.fetch_all()` / `.close()` — FETCH n / FETCH ALL / CLOSE on cursor object
- [ ] 37.35 ⏳ `.each(batch: 1000, fn)` — convenience: cursor + fetch loop + auto-close

#### 37.J — Row-Level Security
- [ ] 37.36 ⏳ `policy on users { name: 'p', using: tenant_id = current_user() }` — CREATE POLICY; auto-filter per user
- [ ] 37.37 ⏳ `users.enable_rls()` / `users.disable_rls()` — ALTER TABLE ENABLE/DISABLE ROW LEVEL SECURITY
- [ ] 37.38 ⏳ `drop policy 'name' on users` — DROP POLICY

#### 37.K — Advisory locks
- [ ] 37.39 ⏳ `advisory_lock(key) { ... }` — block-based advisory lock; auto-release on exit
- [ ] 37.40 ⏳ `advisory_lock_shared(key) { ... }` — shared advisory lock for read-only critical sections
- [ ] 37.41 ⏳ `lock.try_acquire(key)` — non-blocking attempt; returns bool

#### 37.L — Maintenance
- [ ] 37.42 ⏳ `vacuum(users)`, `vacuum(users, full: true, analyze: true)` — VACUUM; reclaims dead MVCC rows
- [ ] 37.43 ⏳ `analyze(users)` — UPDATE STATISTICS for query planner
- [ ] 37.44 ⏳ `reindex(users)`, `reindex(users.email_idx)` — REINDEX table or index
- [ ] 37.45 ⏳ `checkpoint()` — manual WAL checkpoint; flush all dirty pages

#### 37.N — Prepared statements
- [ ] 37.49 ⏳ `prepare('name', users.filter(id = $1).pick(name, email))` — PREPARE; compiles query once, reuses plan
- [ ] 37.50 ⏳ `execute('name', args: [42])` — EXECUTE prepared statement with bound parameters
- [ ] 37.51 ⏳ `deallocate('name')` / `deallocate_all()` — DEALLOCATE; free one or all prepared statements

#### 37.O — Advanced write
- [ ] 37.52 ⏳ `users.filter(...).into_table('archive')` — SELECT INTO; creates new table from query result
- [ ] 37.53 ⏳ `.merge(source, on: key, matched: .update(amount: .new.amount), not_matched: .insert())` — full MERGE statement
- [ ] 37.54 ⏳ `truncate(users, cascade: true)` — TRUNCATE with CASCADE; also truncates dependent FK tables

#### 37.P — Special operations
- [ ] 37.55 ⏳ `users.flashback(before_drop: true)` — restore table from recycle bin (Phase 13.17)
- [ ] 37.56 ⏳ `fiscal_lock('2023')` / `fiscal_unlock('2023')` — lock/unlock fiscal period (Phase 13.11)
- [ ] 37.57 ⏳ `.explain(format: json)` / `.explain(format: text, buffers: true)` — extended EXPLAIN options

#### 37.Q — Real-time change watching
- [ ] 37.61 ⏳ `users.watch()` — returns a live stream of row changes (insert/update/delete); uses WAL CatalogChangeNotifier
- [ ] 37.62 ⏳ `users.watch(filter: active)` — filtered watch; only emits changes matching the condition
- [ ] 37.63 ⏳ `.on('insert', fn)`, `.on('update', fn)`, `.on('delete', fn)` — per-event handlers on watch stream
- [ ] 37.64 ⏳ `users.watch().diff()` — emits `{old, new}` pairs on update; useful for audit trails

#### 37.R — Schemas + multitenancy
- [ ] 37.65 ⏳ `schema('tenant_123').users.filter(active)` — query within a specific schema; compiles to SET search_path or schema-qualified names
- [ ] 37.66 ⏳ `create schema('tenant_123')` / `drop schema('tenant_123', cascade: true)` — CREATE/DROP SCHEMA
- [ ] 37.67 ⏳ `schema('src').users.copy_to(schema: 'dst')` — copy table structure (and optionally data) between schemas

#### 37.S — Sequences
- [ ] 37.68 ⏳ `create sequence('order_num', start: 1000, step: 5)` — CREATE SEQUENCE with options
- [ ] 37.69 ⏳ `sequence('order_num').next()` — NEXTVAL; `sequence('order_num').current()` — CURRVAL; `sequence('order_num').set(500)` — SETVAL
- [ ] 37.70 ⏳ `drop sequence('order_num')` / `alter sequence('order_num', max: 99999)` — DDL on sequences

#### 37.T — Materialized views
- [ ] 37.71 ⏳ `materialized_view('active_users', users.filter(active).pick(id, name))` — CREATE MATERIALIZED VIEW from AxiomQL query
- [ ] 37.72 ⏳ `active_users.refresh()` / `active_users.refresh(concurrent: true)` — REFRESH MATERIALIZED VIEW
- [ ] 37.73 ⏳ `drop materialized_view('active_users')` — DROP MATERIALIZED VIEW
- [ ] 37.74 ⏳ Materialized views are queryable like regular tables: `active_users.filter(name ~ 'A%').count()`

#### 37.U — Schema metadata + comments
- [ ] 37.75 ⏳ `users.comment('Registered application users')` — COMMENT ON TABLE
- [ ] 37.76 ⏳ `users.col('email').comment('Primary contact, must be verified')` — COMMENT ON COLUMN
- [ ] 37.77 ⏳ `users.labels(team: 'auth', domain: 'users')` — key/value labels on tables for tooling and autodoc

#### 37.V — Extensions + statistics
- [ ] 37.78 ⏳ `enable_extension('uuid-ossp')` / `enable_extension('pgvector')` — CREATE EXTENSION; required before using extension types/functions
- [ ] 37.79 ⏳ `disable_extension('name')` — DROP EXTENSION
- [ ] 37.80 ⏳ `list_extensions()` — show available and installed extensions
- [ ] 37.81 ⏳ `statistics('stat_name', users, [age, country])` — CREATE STATISTICS; teaches planner about column correlations for better query plans

#### 37.W — Table inheritance
- [ ] 37.82 ⏳ `create employees extends persons { salary: real, department: text }` — CREATE TABLE ... INHERITS; employees rows appear in persons queries
- [ ] 37.83 ⏳ `persons.only()` — SELECT from parent only, excluding inherited rows → ONLY keyword
- [ ] 37.84 ⏳ `drop table employees (no_inherit: true)` — DROP TABLE without affecting parent

#### 37.M — Quality
- [ ] 37.85 ⏳ Documentation — AxiomQL reference in docs-site: every method with SQL equivalent side-by-side
- [ ] 37.86 ⏳ Fuzz testing — malformed AxiomQL input; every panic = regression test
- [ ] 37.87 ⏳ `.to_sql()` pretty-printer — `users.filter(active).to_sql()` returns the generated SQL (debug + learning tool)

---

> **🏁 FEATURE-COMPLETE CHECKPOINT — week ~120**
> On completing Phase 37, AxiomDB is a complete production database engine with two query interfaces:
> - MySQL + PostgreSQL + OData + GraphQL simultaneously
> - AxiomQL method-chain language as modern alternative to SQL
> - AI-native (embeddings, hybrid search, RAG)
> - Horizontal distribution (sharding + Raft)
> - Deploy on Docker/K8s/systemd
> - Complete documentation and TPC-H published

---

### Phase 38 — AxiomDB-Wasm: Browser Database Engine `⏳` week 122-130

#### 38.A — Wasm compilation target
- [ ] 38.1 ⏳ Compile axiomdb-core, axiomdb-sql, axiomdb-storage to wasm32-wasi — verify all pure-Rust crates compile clean without std::fs / std::net
- [ ] 38.2 ⏳ Feature-gate all OS-dependent code (`#[cfg(not(target_arch = "wasm32"))]`) — mmap, tokio, TCP, file I/O
- [ ] 38.3 ⏳ Wasm-compatible allocator — wee_alloc or dlmalloc for smaller binary size
- [ ] 38.4 ⏳ Binary size budget: ≤200KB gzipped for core engine (parser + executor + B+ Tree + buffer pool)
- [ ] 38.5 ⏳ `cargo build --target wasm32-unknown-unknown` passes clean for engine crates

#### 38.B — OPFS storage backend
- [ ] 38.6 ⏳ `OpfsStorageEngine` — implements StorageEngine trait over Origin Private File System
- [ ] 38.7 ⏳ Synchronous access via `FileSystemSyncAccessHandle` inside Web Worker (read/write/flush at byte offsets)
- [ ] 38.8 ⏳ Page-level I/O: 16KB pages read/written directly to OPFS — same page format as native engine
- [ ] 38.9 ⏳ WAL on OPFS — append-only log file, same format as native WAL, crash recovery on page reload
- [ ] 38.10 ⏳ Storage quota detection — `navigator.storage.estimate()` to warn before hitting browser limits
- [ ] 38.11 ⏳ Fallback to IndexedDB for browsers without OPFS sync access (Safari, older Firefox)

#### 38.C — JavaScript bindings
- [ ] 38.12 ⏳ `wasm-bindgen` API: `AxiomDB.open(name)`, `.execute(sql)`, `.query(sql)` — returns JS objects
- [ ] 38.13 ⏳ Web Worker wrapper — all DB operations run off main thread, communicate via `postMessage`
- [ ] 38.14 ⏳ Promise-based async API: `const rows = await db.query("SELECT * FROM users WHERE id = ?", [42])`
- [ ] 38.15 ⏳ Prepared statements: `const stmt = db.prepare(sql)` — reuse parsed plan, bind params per execution
- [ ] 38.16 ⏳ TypeScript type definitions — full `.d.ts` with generics for query results
- [ ] 38.17 ⏳ npm package: `@axiomdb/browser` — zero dependencies, ESM + CJS, tree-shakeable

#### 38.D — Reactive queries (browser-native)
- [ ] 38.18 ⏳ Live queries: `db.watch("SELECT * FROM todos WHERE done = false", callback)` — callback fires on every INSERT/UPDATE/DELETE that changes the result set
- [ ] 38.19 ⏳ Efficient invalidation — WAL-based change tracking per table, only re-execute watched queries on affected tables
- [ ] 38.20 ⏳ React hook: `useAxiomQuery(sql, params)` — returns reactive state, auto-subscribes/unsubscribes
- [ ] 38.21 ⏳ Vue composable: `useAxiomQuery(sql, params)` — same semantics, Vue reactivity system
- [ ] 38.22 ⏳ Svelte store: `axiomQuery(sql, params)` — Svelte writable store with auto-subscription

#### 38.E — Multi-tab coordination
- [ ] 38.23 ⏳ SharedWorker or BroadcastChannel — single writer across all tabs, prevent OPFS lock conflicts
- [ ] 38.24 ⏳ Tab-aware connection pool — tabs share one DB instance, queries routed to the shared worker
- [ ] 38.25 ⏳ Cross-tab live query notifications — change in tab A triggers reactive update in tab B

#### 38.F — Sync engine (offline-first)
- [ ] 38.26 ⏳ CRDT-based merge — last-write-wins per column with Hybrid Logical Clocks (HLC)
- [ ] 38.27 ⏳ Sync protocol: browser ↔ AxiomDB server — delta sync over WebSocket, only changed rows since last sync
- [ ] 38.28 ⏳ Conflict resolution strategies: LWW (default), server-wins, client-wins, custom merge function
- [ ] 38.29 ⏳ Offline queue — mutations while offline are queued and replayed on reconnect in order
- [ ] 38.30 ⏳ Sync status API: `db.sync.status` → `{ state: 'syncing' | 'synced' | 'offline', pending: 12 }`

#### 38.G — Performance and limits
- [ ] 38.31 ⏳ Benchmark: point lookup latency in Wasm vs native (target: ≤3× native)
- [ ] 38.32 ⏳ Benchmark: INSERT throughput in Wasm+OPFS (target: ≥50K rows/s)
- [ ] 38.33 ⏳ Benchmark: binary size vs sql.js, PGlite, wa-sqlite, DuckDB-Wasm
- [ ] 38.34 ⏳ Benchmark: cold start time (Wasm instantiation + OPFS open, target: <100ms)
- [ ] 38.35 ⏳ Memory pressure handling — graceful eviction when browser signals memory pressure (`performance.measureUserAgentSpecificMemory()`)

#### 38.H — Developer experience
- [ ] 38.36 ⏳ `axiomdb-browser` DevTools extension — inspect tables, run queries, see WAL, monitor sync status
- [ ] 38.37 ⏳ Migration support: `db.migrate(version, upSQL, downSQL)` — versioned schema migrations stored in OPFS
- [ ] 38.38 ⏳ Seed data: `db.seed(sql)` — run initial data script only on first open
- [ ] 38.39 ⏳ Export/import: `db.export()` → ArrayBuffer (full DB file), `AxiomDB.import(buffer)` — portable backups
- [ ] 38.40 ⏳ Encryption at rest: `AxiomDB.open(name, { encryption: key })` — AES-256-GCM per page, key never touches disk

#### 38.I — Quality
- [ ] 38.41 ⏳ Integration tests in Playwright — real browser (Chrome, Firefox, Safari), real OPFS
- [ ] 38.42 ⏳ Stress test: 100K rows INSERT + SELECT + live query in single tab
- [ ] 38.43 ⏳ Multi-tab stress test: 4 tabs writing concurrently, verify consistency
- [ ] 38.44 ⏳ Sync integration test: browser ↔ AxiomDB server, network interruption, reconnect, verify convergence
- [ ] 38.45 ⏳ Documentation — user guide: getting started, React/Vue/Svelte integration, sync setup, migration guide
- [ ] 38.46 ⏳ Documentation — internals: OPFS storage engine, Wasm compilation, sync protocol, CRDT implementation

---

> **🏁 FULL PLATFORM CHECKPOINT — week ~130**
> On completing Phase 38, AxiomDB runs everywhere:
> - Server: Linux/macOS/Windows via TCP (MySQL wire protocol)
> - Embedded: desktop (Tauri, Electron), mobile (Flutter, React Native)
> - Browser: Wasm + OPFS, offline-first, reactive queries, multi-tab, sync
> - Same engine, same SQL, same MVCC — every target

---


## Oracle-specific JSON features (moved from Phase 11)

Ni MySQL ni PostgreSQL tienen equivalente nativo. Se re-evalúan cuando haga falta compatibilidad con Oracle.

### 11.23c — `CREATE JSON SCHEMA` DDL + catalog
- [ ] Catalog storage para esquemas JSON reutilizables nombrados (`CREATE JSON SCHEMA ...` DDL + metadata) y `CHECK (JSON_SCHEMA_VALID(<schema_name>, col))` resolviendo desde el catálogo.
- **Motivo wishlist:** Oracle 21c tiene JSON Schema como tipo catálogo; PG soporta schema validation vía extensión `pg_jsonschema` pero sin DDL nativo; MySQL no lo tiene. La función `JSON_SCHEMA_VALID(literal_schema, doc)` (11.23a-f ✅) ya cubre el uso común sin catalog persistence.

### 11.24c — Dot notation para columnas JSON
- [ ] `t.doc.a.b` como azúcar para `JSON_EXTRACT(t.doc, '$.a.b')`; requiere parser disambiguation vs schema-qualified names (`schema.table.col`).
- **Motivo wishlist:** Oracle-specific (Oracle 12c+ / 21c). MySQL usa `t.doc->'$.a.b'` y `t.doc->>'$.a.b'`; PostgreSQL usa `t.doc->'a'->'b'` y `t.doc->>'a'->>'b'`. Ambos syntaxes ya están implementados en 11.16 / 11.18a. El azúcar dot-notation no agrega capacidad funcional.

## Moved from progreso.md (MySQL+PG refocus 2026-04-13)

Items no estrictamente necesarios para paridad MySQL+PG. Mayoría Oracle/SQL:2011/nicho:

### Phase 12 — JIT
- [ ] 12.3 ⏳ Basic JIT with LLVM — compile simple predicates to native code

### Phase 13 — Oracle / SQL:2011 specific
- [ ] 13.10 ⏳ Gapless sequences — `CREATE SEQUENCE inv_num GAPLESS START 1`; unlike AUTO_INCREMENT (which skips numbers on rollback), a gapless sequence uses a dedicated lock + WAL entry to guarantee no gaps even across failures; `NEXTVAL('inv_num')` blocks until the sequence number is committed; required by tax law in most countries for invoice numbering; `LAST_VALUE`, `RESET TO n` for administration
- [ ] 13.11 ⏳ Fiscal period locking — `LOCK FISCAL PERIOD '2023'`; after locking, INSERT/UPDATE/DELETE of rows with any date column falling within that period returns an error; `UNLOCK FISCAL PERIOD '2023'` for corrections; stored in a system table `axiom_locked_periods`; the executor checks against it for tables that have a designated date column (`CREATE TABLE t (..., WITH FISCAL_DATE = created_at)`)
- [ ] 13.15 ⏳ Filtered LISTEN/NOTIFY — `SUBSCRIBE TO orders WHERE status = 'pending' AND total > 1000 ON CHANGE`; current LISTEN/NOTIFY (13.4) notifies any change to the entire table; real-time dashboards need selective subscriptions — "notify me only about high-value pending orders" — without this the client receives all changes and filters in application code, wasting network bandwidth
- [ ] 13.16 ⏳ Transactional reservations with auto-release
- [ ] 13.17 ⏳ Recycle Bin for DROP TABLE — `DROP TABLE clientes` moves the table to the recycle bin instead of deleting it immediately; `FLASHBACK TABLE clientes TO BEFORE DROP` restores it completely with all data, indexes, and constraints intact; `SELECT * FROM axiom_recyclebin` lists dropped objects; `PURGE TABLE clientes` permanently deletes from the bin; configurable `recyclebin_retention = '30 days'`; eliminates the most common DBA emergency ("someone accidentally dropped the wrong table in production") without requiring a full database restore; Oracle introduced this in 10g and it became one of the most appreciated features
- [ ] 13.18b ⏳ Historical reads — `BEGIN READ ONLY AS OF TIMESTAMP '2023-12-31 23:59:59'` anchors the snapshot to a past point in time; MVCC already stores the data (Phase 7), this adds the SQL syntax and executor support; critical for auditing financial data at a specific date without exporting; precursor to the full bi-temporal model in 13.18 (moved from 7.16)
- [ ] 13.18 ⏳ Bi-temporal tables (SQL:2011) — first-class DDL for two-time-dimension data: `PERIOD FOR validity (valid_from, valid_until)` (application time: when the fact was true in reality) + `PERIOD FOR system_time` (transaction time: when it was recorded); `SELECT * FROM salaries FOR PERIOD OF validity AS OF DATE '2023-01-01' AS OF SYSTEM TIME '2023-02-15'` answers "what salary did Alice have on Jan 1 according to the records as they existed on Feb 15?"; extends Phase 7.16 (read-only AS OF) to a full SQL:2011 bitemporal model with DDL support; critical for accounting, insurance, HR, legal — any domain where both "when it happened" and "when we knew about it" matter independently — `INSERT INTO reservations (resource_id, session_id) VALUES (42, 'sess_abc') ON CONFLICT DO NOTHING RETURNING CASE WHEN id IS NULL THEN 'unavailable' ELSE 'reserved' END`; plus automatic release when session expires or connection drops; hotel booking, concert tickets, parking spots, inventory hold — "hold this item for 15 minutes while the user checks out"

### Phase 14 — Time-series / IoT / CAS BLOB (keeps partitioning 14.1/14.2 in core)
- [ ] 14.3 ⏳ Automatic compression of historical partitions — LZ4 columnar
- [ ] 14.4 ⏳ Continuous aggregates — incremental refresh of only the new delta
- [ ] 14.5 ⏳ TTL per row — `WITH TTL 3600` + background reaper in Tokio
- [ ] 14.6 ⏳ LRU eviction — for in-memory mode with RAM limit
- [ ] 14.7 ⏳ Chunk-level compression statistics — track compression ratio per partition; decides when to compress automatically
- [ ] 14.8 ⏳ Time-series benchmarks — insert 1M rows with timestamp; compare range scan vs TimescaleDB
- [ ] 14.9 ⏳ Content-addressed BLOB store — SHA256 of blob bytes = content key; separate content-store area in the .db file (beyond the heap); on BLOB insert: compute SHA256 → lookup in content index → if found: increment ref_count + store only the 32-byte hash in the BLOB_REF (header=0x02) → if not found: write bytes once + ref_count=1; two rows with identical photo share exactly one copy on disk; transparent to SQL layer — `SELECT photo` returns the full bytes regardless of backend
- [ ] 14.10 ⏳ BLOB garbage collector — periodic scan of content store ref_counts; blobs with ref_count=0 are reclaimed; integrates with MVCC vacuum cycle (runs after dead-tuple vacuum so rollback of inserts correctly decrements); safe under concurrent reads (ref_count never drops to 0 while a snapshot can see the blob)
- [ ] 14.11 ⏳ BLOB dedup metrics — `SELECT * FROM axiom_blob_stats` returns: `total_blobs`, `unique_blobs`, `dedup_ratio`, `bytes_saved`, `avg_blob_size`; helps users understand storage efficiency and decide whether to enable/disable dedup per table (`WITH (blob_dedup = off)`)
- [ ] 14.12 ⏳ IoT: LAST(value ORDER BY ts) aggregate — returns the most recent value per group ordered by timestamp; `SELECT device_id, LAST(temperature ORDER BY recorded_at) FROM readings GROUP BY device_id`; different from MAX; essential for "current state" dashboards of sensors, vehicles, wearables
- [ ] 14.13 ⏳ IoT: Dead-band / change-only recording — `CREATE TABLE sensors WITH (dead_band_col = temp, dead_band = 0.5)`; engine skips INSERT when value differs from previous by less than threshold; reduces storage 80-95% for slowly-changing sensors without any application changes
- [ ] 14.14 ⏳ IoT: Gap filling and interpolation — `INTERPOLATE(value, 'locf' | 'linear' | 'step')` fills NULL gaps from sensor disconnections; LOCF = last observation carried forward; essential for charting and ML pipelines that require continuous time series
- [ ] 14.15 ⏳ IoT: EVERY interval syntax — `SELECT AVG(temp) EVERY '5 minutes' FROM sensors WHERE ts > NOW() - INTERVAL '1 day'`; declarative downsampling without explicit GROUP BY FLOOR(EXTRACT(EPOCH FROM ts)/300); reduces query complexity for time-bucketed analytics

### Phase 16 — Lua/WASM runtimes + Oracle autonomous txns
- [ ] 16.4 ⏳ Lua runtime — `mlua`, EVAL with atomic `query()` and `execute()`
- [ ] 16.5 ⏳ WASM runtime — `wasmtime`, sandbox, memory limits and timeout
- [ ] 16.6 ⏳ CREATE FUNCTION LANGUAGE wasm FROM FILE — load .wasm plugin
- [ ] 16.10b ⏳ Autonomous transactions — `PRAGMA AUTONOMOUS_TRANSACTION` on a stored procedure makes it run in an independent transaction; `COMMIT` inside commits only that procedure's changes; if outer transaction does `ROLLBACK`, the autonomous transaction's changes are preserved; critical for audit logging that persists even when the main operation fails; requires 16.7 (stored procedures) first (moved from 7.20) — Pgbouncer-equivalent implemented inside the engine; multiplexes N application connections into M database backend connections (N >> M); transaction-mode pooling (connection returned to pool after each COMMIT/ROLLBACK); session variables reset between borrows; eliminates the need for external Pgbouncer/Pgpool deployment; critical for any app with >100 concurrent users since creating one OS thread per TCP connection does not scale

### Phase 17 — Advanced security (compliance-specific)
- [ ] 17.15 ⏳ Column-level encryption — `CREATE TABLE patients (name TEXT, ssn TEXT ENCRYPTED WITH KEY 'k1')`; encryption/decryption happens inside the engine using AES-256-GCM; ciphertext stored on disk; plaintext only visible in query results to authorized roles; key rotation without full table rewrite; healthcare (HIPAA), HR, legal all require this for PII fields
- [ ] 17.16 ⏳ Dynamic data masking — `CREATE MASKING POLICY mask_ssn ON patients (ssn) USING MASKED WITH ('***-**-' || RIGHT(ssn,4))`; different roles see different representations of the same column without changing stored data; `SELECT ssn FROM patients` returns real value to admins, masked value to analysts; no application code changes required
- [ ] 17.18 ⏳ Consent-based row access — `CREATE POLICY patient_consent ON records USING (has_consent(patient_id, CURRENT_USER))`; patient explicitly grants a specific doctor access to their records; revoking consent immediately removes access; beyond standard RLS — the USING expression calls a user-defined consent table
- [ ] 17.19 ⏳ GDPR physical purge — `DELETE PERMANENTLY FROM patients WHERE id = 42 PURGE ALL VERSIONS`; with MVCC, normal DELETE leaves historical versions visible to old snapshots; PURGE physically overwrites all pages containing that row's versions across all WAL history; required for GDPR right-to-erasure and CCPA; audit entry records the purge but not the data
- [ ] 17.20 ⏳ Digital signatures on rows — `SELECT SIGN_ROW(contract_id) FROM contracts` embeds an HMAC of the row's content + timestamp + signer_id; `VERIFY_ROW(contract_id)` returns TRUE if content matches signature; tamper detection for legal documents, audit logs, financial records; signatures stored alongside the row in the heap
- [ ] 17.21 ⏳ Storage quotas per tenant — `ALTER TENANT acme SET (max_storage = '10 GB', max_rows = 1000000)`;

### Phase 22b — Platform extras (lineage, queue, DAG, result cache)
- [ ] 22b.7 ⏳ Data lineage tracking — `SELECT * FROM axiom_lineage WHERE table_name = 'ml_features'` shows which tables fed this one and when; `CREATE TABLE ml_features AS SELECT ... FROM raw_events WITH LINEAGE`; tracks column-level derivations across transformations; ML pipelines need to know which training data produced which model; compliance systems need to trace PII through all derived tables; enables impact analysis ("if I change this source table, what downstream tables break?")
- [ ] 22b.8 ⏳ Query result cache with auto-invalidation — `SELECT /*+ RESULT_CACHE */ * FROM products WHERE featured = TRUE`; engine caches the result set and automatically invalidates it when any of the underlying tables changes (not just TTL-based); `SELECT /*+ RESULT_CACHE(ttl=60s) */ ...` for TTL fallback; `SELECT * FROM axiom_result_cache` shows cached queries, hit rate, memory used; smarter than Phase 22b.8 original (TTL only) — inspired by Oracle SQL Result Cache which invalidates on data change: no stale data, no manual INVALIDATE needed
- [ ] 22b.9 ⏳ Transactional Message Queue — `CREATE QUEUE pagos_pendientes`; `ENQUEUE(queue=>'pagos_pendientes', message=>pago_record)` inside a transaction: the message is only visible to consumers when the surrounding COMMIT succeeds; if the transaction rolls back, the message never appears; `DEQUEUE(queue=>'pagos_pendientes')` removes and returns the next message atomically; `max_retries=3` + dead letter queue `pagos_fallidos` after N failed attempts; `message_delay = INTERVAL '5 minutes'` for delayed delivery; ACID semantics throughout — fundamentally different from LISTEN/NOTIFY (which is fire-and-forget, not persistent, not transactional); enables: payment processing, order fulfillment, async email sending, workflow orchestration — all with exactly-once delivery guarantees
- [ ] 22b.10 ⏳ Job Chains with DAG scheduling — `CREATE CHAIN etl_noche` defines a directed acyclic graph of jobs: step A runs first, then B and C run in parallel when A succeeds, then D runs only when both B and C succeed, then E always runs (cleanup) regardless of success/failure; `ON_ERROR = 'continue'|'abort_chain'|'skip_to'` per step; retry with exponential backoff; timeout per step; notification on chain failure via the transactional queue (22b.9); `SELECT * FROM axiom_chain_runs` shows execution history with per-step timing; far more powerful than cron-style scheduling (22b.1) — enables complex ETL pipelines, multi-step data processing, database-native workflow orchestration

