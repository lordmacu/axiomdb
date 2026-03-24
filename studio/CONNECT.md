# AxiomStudio — Connection Guide

Complete map of every mock data source in AxiomStudio and the real AxiomDB API
endpoint/SDK call it needs to replace. All current mock data lives in `lib/mock.ts`.

**Status:** UI complete with mock data. Wire-up requires Phase 8 (HTTP API / wire protocol).

---

## Base URL

```
http://localhost:8080/_api/   ← AxiomDB HTTP API (Phase 8)
```

All requests accept and return JSON. Authentication via session token in header:
`Authorization: Bearer <token>`

---

## 1. Dashboard (`app/page.tsx`)

### Metrics cards
**Mock:** `METRICS` constant in `lib/mock.ts`
**Real:** `GET /_api/metrics`
```typescript
// Response
{
  queries_per_second: number
  active_connections: number
  max_connections: number
  db_size_bytes: number        // format as MB/GB on frontend
  cache_hit_rate: number       // 0-100
  uptime_seconds: number       // format as "Xd Xh Xm" on frontend
  wal_size_bytes: number
  avg_query_time_ms: number
}
```

### Sparklines (live refresh)
**Mock:** `SPARKLINE_DATA` in `lib/mock.ts`
**Real:** `GET /_api/metrics/history?metric=qps,connections,cache&points=10`
```typescript
{ qps: number[], connections: number[], cache: number[] }
```

### Recent queries table
**Mock:** `QUERY_LOG` in `lib/mock.ts`
**Real:** `GET /_api/queries/recent?limit=10`
```typescript
Array<{ query: string; duration_ms: number; rows: number; timestamp: string; status: 'ok' | 'error' }>
```

### Slow queries section
**Mock:** hardcoded in `app/page.tsx`
**Real:** `GET /_api/queries/slow?threshold_ms=100&limit=5`
```typescript
Array<{ query: string; duration_ms: number; table: string; timestamp: string }>
```

---

## 2. Query Editor (`app/query/page.tsx`)

### Execute query
**Mock:** `setTimeout` + returns `MOCK_RESULTS.default`
**Real:** `POST /_api/query`
```typescript
// Request
{ sql?: string; axiomql?: string; variables?: Record<string, string> }

// Response
{
  columns: Array<{ name: string; type: string; nullable: boolean }>
  rows: Array<Record<string, unknown>>
  duration_ms: number
  rows_affected?: number       // for INSERT/UPDATE/DELETE
}

// Error response
{ error: string; sqlstate: string; position?: number; hint?: string }
```

### EXPLAIN plan
**Mock:** button exists, no action
**Real:** `POST /_api/explain`
```typescript
// Request
{ sql?: string; axiomql?: string; analyze?: boolean }

// Response
{
  plan: ExplainNode   // tree structure
  total_cost: number
  actual_time_ms?: number  // only when analyze: true
}

type ExplainNode = {
  node_type: string         // 'SeqScan' | 'IndexScan' | 'HashJoin' | ...
  table?: string
  index?: string
  cost: number
  rows: number
  children?: ExplainNode[]
  filter?: string
  actual_time_ms?: number
}
```

### Autocompletion (Monaco)
**Mock:** `TABLE_NAMES` and `SCHEMA_COMPLETIONS` hardcoded in `app/query/page.tsx`
**Real:** `GET /_api/tables` for table names + `GET /_api/tables/:name/schema` for columns
Register completion providers on connect, refresh on schema change notification.

### SQL ↔ AxiomQL translator
**Mock:** heuristic regex-based translator in `app/query/page.tsx` (`sqlToAxiomql`, `axiomqlToSql`)
**Real (Phase 36):** `POST /_api/translate`
```typescript
// Request
{ sql: string } | { axiomql: string }
// Response
{ sql: string } | { axiomql: string }
```

---

## 3. Tables browser (`app/tables/page.tsx`)

### Table list
**Mock:** `TABLES` array in `lib/mock.ts`
**Real:** `GET /_api/tables`
```typescript
Array<{
  name: string
  type: 'table' | 'view'
  rows: number
  size_bytes: number
  last_updated: string    // ISO timestamp
  schema: string          // schema name, e.g. 'public'
}>
```

---

## 4. Table detail (`app/tables/[table]/page.tsx`)

### Load data (Data tab)
**Mock:** `TABLE_DATA[tableName]` in `app/tables/[table]/page.tsx`
**Real:** `GET /_api/tables/:name/data?page=0&limit=50&sort=id&order=asc`
```typescript
{
  rows: Array<Record<string, unknown>>
  total: number
  page: number
  limit: number
}
```

### Filter rows
**Mock:** client-side filter on loaded rows
**Real:** `GET /_api/tables/:name/data?filter=col:value&page=0&limit=50`
```typescript
// URL params: filter=name:Alice or filter=age:30
// Server applies the filter efficiently (uses index if available)
```

### Edit row (inline edit)
**Mock:** updates local state, shows SQL preview
**Real:** `PUT /_api/tables/:name/rows/:id`
```typescript
// Request body: { col: value }
// Response: { row: Record<string, unknown> }  ← updated row
```

### Delete row
**Mock:** removes from local state, shows SQL preview
**Real:** `DELETE /_api/tables/:name/rows/:id`
```typescript
// Response: { affected: 1 }
```

### Add row
**Mock:** appends to local state
**Real:** `POST /_api/tables/:name/rows`
```typescript
// Request body: { col: value, ... }
// Response: { row: Record<string, unknown>; id: unknown }  ← inserted row with generated id
```

### Load schema (Schema tab)
**Mock:** `SCHEMAS[tableName]` in `lib/mock.ts`
**Real:** `GET /_api/tables/:name/schema`
```typescript
{
  columns: Array<{
    name: string; type: string; nullable: boolean
    default: string | null; pk: boolean; fk: string | null
  }>
  indexes: Array<{ name: string; columns: string[]; unique: boolean; type: string }>
}
```

### Change column type / nullable
**Mock:** local state update
**Real:** `PATCH /_api/tables/:name/columns/:col`
```typescript
// Request: { type?: string; nullable?: boolean }
// Generates: ALTER TABLE name ALTER COLUMN col TYPE newtype
```

### FK management (add/edit/remove)
**Mock:** local state
**Real:** `PATCH /_api/tables/:name/columns/:col/fk`
```typescript
// Add/edit: { references: 'other_table.col' }
// Remove:   { references: null }
// Generates: ALTER TABLE ... ADD/DROP CONSTRAINT fk_...
```

### Create index
**Mock:** appends to local indexes state
**Real:** `POST /_api/tables/:name/indexes`
```typescript
// Request: { name: string; columns: string[]; unique: boolean }
// Response: { index: IndexInfo }
```

### Drop index
**Mock:** removes from local state
**Real:** `DELETE /_api/tables/:name/indexes/:indexName`

### Copy DDL
**Mock:** generates from `SCHEMAS` client-side
**Real:** `GET /_api/tables/:name/ddl` → `{ ddl: string }` (server generates exact CREATE TABLE)

---

## 5. Settings (`app/settings/page.tsx`)

### Load current config
**Mock:** hardcoded initial values
**Real:** `GET /_api/config`
```typescript
{
  connection: { host, port, database, user }
  engine: { wal_enabled, fsync, max_connections, max_wal_size_mb, log_level }
  // studio prefs are localStorage-only, never sent to server
}
```

### Save engine config
**Mock:** setTimeout + "Saved" toast
**Real:** `PUT /_api/config/engine`
```typescript
// Request: { wal_enabled?, fsync?, max_connections?, max_wal_size_mb?, log_level? }
// Response: { ok: true } | { error: string }
```

### Test connection
**Mock:** setTimeout + always returns 'ok'
**Real:** `POST /_api/connections/test`
```typescript
// Request: { host, port, database, user, password, ssl }
// Response: { ok: true; latency_ms: number } | { ok: false; error: string }
```

### Version info
**Mock:** hardcoded "AxiomDB v0.1.0-dev (Phase 4)"
**Real:** `GET /_api/version`
```typescript
{ version: string; phase: number; build: string; git_sha: string }
```

---

## 6. Real-time features (Phase 8+)

### Reactive queries (`.watch()`)
**Mock:** not implemented
**Real:** WebSocket `ws://localhost:8080/_ws/watch`
```typescript
// Subscribe:
{ action: 'subscribe'; table: string; filter?: string }

// Server pushes events:
{ event: 'insert' | 'update' | 'delete'; row: Record<string, unknown>; old_row?: Record<string, unknown> }

// Unsubscribe:
{ action: 'unsubscribe'; table: string }
```

### LISTEN/NOTIFY
**Mock:** not implemented
**Real:** WebSocket `ws://localhost:8080/_ws/notify`
```typescript
// Subscribe: { action: 'listen'; channel: string }
// Event:     { channel: string; payload: string; pid: number }
```

---

## 7. How to wire up (checklist for Phase 8)

When Phase 8 (HTTP API) is ready:

1. Create `lib/api.ts` — base `fetch` wrapper with error handling, auth header, base URL from settings
2. Replace `lib/mock.ts` imports with `lib/api.ts` calls one file at a time
3. Start with `Dashboard` (read-only, safe) → `Tables browser` → `Table detail` → `Query Editor`
4. Keep mock as fallback when `/_api/` is unreachable (dev mode without server running)
5. Update Monaco completion providers to fetch from real schema on mount
6. Replace heuristic SQL↔AxiomQL translator with `POST /_api/translate` (Phase 36)
7. Add WebSocket connection manager for real-time features

**Connection string format (stored in Settings):**
```
axiomdb://user:password@host:port/database?ssl=true
```

---

## 8. Studio prefs that are client-only (never go to server)

These live in `localStorage` only and do NOT need a server endpoint:
- `axiomstudio_history` — query history (last 20)
- `axiomstudio_saved` — saved queries / bookmarks
- Studio preferences: font size, tab size, word wrap, minimap, default language, theme
