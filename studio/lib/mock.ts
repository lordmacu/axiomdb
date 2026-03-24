export type TableInfo = {
  name: string
  rows: number
  size: string
  lastUpdated: string
  type: 'table' | 'view'
}

export type Column = {
  name: string
  type: string
  nullable: boolean
  default: string | null
  pk: boolean
  fk: string | null
}

export type IndexInfo = {
  name: string
  columns: string[]
  unique: boolean
  type: string
}

export type UserRow = {
  id: number; name: string; email: string; age: number
  active: boolean; created_at: string
}

export type OrderRow = {
  id: number; user_id: number; amount: number
  status: string; created_at: string
}

export type QueryLog = {
  query: string; duration: number; rows: number; timestamp: string; status: 'ok' | 'error'
}

// Tables
export const TABLES: TableInfo[] = [
  { name: 'users', rows: 10234, size: '2.1 MB', lastUpdated: '2026-03-23 18:42:11', type: 'table' },
  { name: 'orders', rows: 51847, size: '8.4 MB', lastUpdated: '2026-03-23 18:45:03', type: 'table' },
  { name: 'products', rows: 523, size: '128 KB', lastUpdated: '2026-03-22 09:15:44', type: 'table' },
  { name: 'categories', rows: 20, size: '8 KB', lastUpdated: '2026-03-20 14:22:30', type: 'table' },
  { name: 'active_users', rows: 8102, size: '—', lastUpdated: '2026-03-23 18:45:03', type: 'view' },
]

// Schemas
export const SCHEMAS: Record<string, Column[]> = {
  users: [
    { name: 'id', type: 'INT', nullable: false, default: null, pk: true, fk: null },
    { name: 'name', type: 'TEXT', nullable: false, default: null, pk: false, fk: null },
    { name: 'email', type: 'TEXT', nullable: false, default: null, pk: false, fk: null },
    { name: 'age', type: 'INT', nullable: true, default: null, pk: false, fk: null },
    { name: 'active', type: 'BOOL', nullable: false, default: 'true', pk: false, fk: null },
    { name: 'created_at', type: 'TIMESTAMP', nullable: false, default: 'now()', pk: false, fk: null },
  ],
  orders: [
    { name: 'id', type: 'INT', nullable: false, default: null, pk: true, fk: null },
    { name: 'user_id', type: 'INT', nullable: false, default: null, pk: false, fk: 'users.id' },
    { name: 'amount', type: 'REAL', nullable: false, default: null, pk: false, fk: null },
    { name: 'status', type: 'TEXT', nullable: false, default: "'pending'", pk: false, fk: null },
    { name: 'created_at', type: 'TIMESTAMP', nullable: false, default: 'now()', pk: false, fk: null },
  ],
  products: [
    { name: 'id', type: 'INT', nullable: false, default: null, pk: true, fk: null },
    { name: 'name', type: 'TEXT', nullable: false, default: null, pk: false, fk: null },
    { name: 'price', type: 'REAL', nullable: false, default: null, pk: false, fk: null },
    { name: 'stock', type: 'INT', nullable: false, default: '0', pk: false, fk: null },
    { name: 'category_id', type: 'INT', nullable: true, default: null, pk: false, fk: 'categories.id' },
  ],
  categories: [
    { name: 'id', type: 'INT', nullable: false, default: null, pk: true, fk: null },
    { name: 'name', type: 'TEXT', nullable: false, default: null, pk: false, fk: null },
    { name: 'slug', type: 'TEXT', nullable: false, default: null, pk: false, fk: null },
  ],
}

export const INDEXES: Record<string, IndexInfo[]> = {
  users: [
    { name: 'users_pkey', columns: ['id'], unique: true, type: 'B-Tree' },
    { name: 'idx_users_email', columns: ['email'], unique: true, type: 'B-Tree' },
    { name: 'idx_users_active', columns: ['active', 'created_at'], unique: false, type: 'B-Tree' },
  ],
  orders: [
    { name: 'orders_pkey', columns: ['id'], unique: true, type: 'B-Tree' },
    { name: 'idx_orders_user_id', columns: ['user_id'], unique: false, type: 'B-Tree' },
    { name: 'idx_orders_status', columns: ['status', 'created_at'], unique: false, type: 'B-Tree' },
  ],
}

const NAMES = ['Alice Chen', 'Bob Martinez', 'Carlos Silva', 'Diana Park', 'Ethan Johnson',
  'Fiona Walsh', 'Gabriel Torres', 'Hannah Kim', 'Ivan Petrov', 'Julia Santos',
  'Kevin Nguyen', 'Laura Müller', 'Marco Rossi', 'Nadia Hassan', 'Oscar Brown',
  'Paula Fernandez', 'Quinn Murphy', 'Rosa Lee', 'Samuel Adams', 'Tina Patel',
  'Uma Sharma', 'Victor Hugo', 'Wendy Clark', 'Xavier Bell', 'Yasmin Ali',
  'Zara Scott', 'Aiden Brooks', 'Bella Watson', 'Cole Harris', 'Daisy Morgan']

export function generateUsers(n = 50): UserRow[] {
  return Array.from({ length: n }, (_, i) => ({
    id: i + 1,
    name: NAMES[i % NAMES.length],
    email: `${NAMES[i % NAMES.length].toLowerCase().replace(' ', '.')}${i > 29 ? i : ''}@example.com`,
    age: 20 + (i * 7 % 45),
    active: i % 7 !== 0,
    created_at: new Date(Date.now() - i * 86400000 * 3).toISOString().replace('T', ' ').slice(0, 19),
  }))
}

const STATUSES = ['completed', 'completed', 'completed', 'pending', 'failed', 'processing']
export function generateOrders(n = 100): OrderRow[] {
  return Array.from({ length: n }, (_, i) => ({
    id: i + 1,
    user_id: 1 + (i * 3 % 50),
    amount: Math.round((9.99 + i * 13.37) * 100) / 100,
    status: STATUSES[i % STATUSES.length],
    created_at: new Date(Date.now() - i * 3600000 * 5).toISOString().replace('T', ' ').slice(0, 19),
  }))
}

export const QUERY_LOG: QueryLog[] = [
  { query: 'SELECT * FROM users WHERE active = TRUE LIMIT 100', duration: 4, rows: 100, timestamp: '18:45:03', status: 'ok' },
  { query: "SELECT COUNT(*) FROM orders WHERE status = 'pending'", duration: 2, rows: 1, timestamp: '18:44:51', status: 'ok' },
  { query: "UPDATE users SET active = FALSE WHERE last_login < NOW() - INTERVAL '90 days'", duration: 143, rows: 234, timestamp: '18:43:22', status: 'ok' },
  { query: 'SELECT u.name, SUM(o.amount) FROM users u JOIN orders o ON u.id = o.user_id GROUP BY u.name', duration: 28, rows: 1847, timestamp: '18:41:09', status: 'ok' },
  { query: "INSERT INTO orders (user_id, amount, status) VALUES (42, 99.99, 'pending')", duration: 6, rows: 1, timestamp: '18:40:33', status: 'ok' },
]

export const METRICS = {
  queriesPerSecond: 2847,
  activeConnections: 12,
  maxConnections: 50,
  dbSize: '1.2 GB',
  cacheHitRate: 94.2,
  uptime: '14d 6h 23m',
  walSize: '48 MB',
  avgQueryTime: 8.3,
}

export const SPARKLINE_DATA = {
  qps: [1200, 1800, 2100, 2400, 2200, 2847, 2700, 2900, 2847],
  connections: [8, 9, 11, 10, 12, 11, 13, 12, 12],
  cache: [91, 92, 93, 92, 94, 93, 95, 94, 94.2],
}
