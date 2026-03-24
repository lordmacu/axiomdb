'use client'
import { useState, useEffect, useRef, useCallback } from 'react'
import { Activity, Database, Zap, Wifi, Clock, TrendingUp, AlertTriangle } from 'lucide-react'
import { METRICS, QUERY_LOG, SPARKLINE_DATA, type QueryLog } from '@/lib/mock'
import { formatNumber } from '@/lib/utils'

// ── Live refresh intervals ────────────────────────────────────────────────────

type Interval = 5 | 10 | 30 | 'off'
const INTERVAL_OPTIONS: { label: string; value: Interval }[] = [
  { label: '5s', value: 5 },
  { label: '10s', value: 10 },
  { label: '30s', value: 30 },
  { label: 'Off', value: 'off' },
]

// ── Slow query mock data ──────────────────────────────────────────────────────

type SlowQuery = {
  query: string
  duration: number
  table: string
  timestamp: string
}

const SLOW_QUERIES: SlowQuery[] = [
  {
    query: "SELECT u.*, COUNT(o.id) FROM users u LEFT JOIN orders o ON u.id = o.user_id GROUP BY u.id ORDER BY created_at DESC",
    duration: 342,
    table: 'users',
    timestamp: '18:39:11',
  },
  {
    query: "UPDATE orders SET status = 'archived' WHERE created_at < NOW() - INTERVAL '365 days'",
    duration: 187,
    table: 'orders',
    timestamp: '18:31:55',
  },
  {
    query: "SELECT * FROM orders o JOIN users u ON o.user_id = u.id WHERE o.amount > 1000 AND u.active = TRUE",
    duration: 124,
    table: 'orders',
    timestamp: '18:22:03',
  },
]

// ── Fake recent-query entries for live mode ───────────────────────────────────

const LIVE_QUERIES: QueryLog[] = [
  { query: 'SELECT * FROM users WHERE active = TRUE LIMIT 20', duration: 3, rows: 20, timestamp: '', status: 'ok' },
  { query: "SELECT COUNT(*) FROM orders WHERE status = 'pending'", duration: 2, rows: 1, timestamp: '', status: 'ok' },
  { query: 'SELECT id, name FROM users ORDER BY name LIMIT 10', duration: 5, rows: 10, timestamp: '', status: 'ok' },
  { query: "INSERT INTO orders (user_id, amount, status) VALUES (7, 49.99, 'pending')", duration: 8, rows: 1, timestamp: '', status: 'ok' },
]

// ── Helpers ───────────────────────────────────────────────────────────────────

function jitter(n: number): number {
  return Math.round(n * (1 + (Math.random() - 0.5) * 0.1))
}

function nowTime(): string {
  return new Date().toLocaleTimeString('en-US', { hour12: false })
}

// ── MetricCard ────────────────────────────────────────────────────────────────

function MetricCard({ icon: Icon, label, value, sub, trend }: {
  icon: React.ComponentType<{ className?: string }>
  label: string
  value: string
  sub?: string
  trend?: string
}) {
  return (
    <div className="bg-surface border border-border rounded-lg p-4 flex flex-col gap-3">
      <div className="flex items-center justify-between">
        <span className="text-xs text-text-secondary">{label}</span>
        <div className="w-6 h-6 rounded flex items-center justify-center bg-accent/10">
          <Icon className="w-3.5 h-3.5 text-accent" />
        </div>
      </div>
      <div>
        <div className="text-2xl font-semibold text-text-primary tracking-tight">{value}</div>
        {sub && <div className="text-xs text-text-secondary mt-0.5">{sub}</div>}
      </div>
      {trend && <div className="text-xs text-accent">{trend}</div>}
    </div>
  )
}

// ── Sparkline ─────────────────────────────────────────────────────────────────

function Sparkline({ data }: { data: number[] }) {
  const max = Math.max(...data)
  const min = Math.min(...data)
  const range = max - min || 1
  const h = 32
  const w = 80
  const points = data.map((v, i) => {
    const x = (i / (data.length - 1)) * w
    const y = h - ((v - min) / range) * h
    return `${x},${y}`
  }).join(' ')
  return (
    <svg width={w} height={h} className="opacity-60">
      <polyline fill="none" stroke="#10b981" strokeWidth="1.5"
        strokeLinejoin="round" strokeLinecap="round" points={points} />
    </svg>
  )
}

// ── Dashboard ─────────────────────────────────────────────────────────────────

export default function Dashboard() {
  // Live mode state
  const [live, setLive] = useState(false)
  const [interval, setInterval_] = useState<Interval>(10)
  const [tick, setTick] = useState(0)
  const [lastRefresh, setLastRefresh] = useState<string>(() => nowTime())
  const [recentLog, setRecentLog] = useState<QueryLog[]>(QUERY_LOG)
  const timerRef = useRef<ReturnType<typeof setTimeout> | null>(null)

  // Jittered metrics (update on each tick)
  const [qps, setQps] = useState(METRICS.queriesPerSecond)
  const [connections, setConnections] = useState(METRICS.activeConnections)
  const [cacheHit, setCacheHit] = useState(METRICS.cacheHitRate)

  const refresh = useCallback(() => {
    setQps(jitter(METRICS.queriesPerSecond))
    setConnections(jitter(METRICS.activeConnections))
    setCacheHit(Math.round(jitter(METRICS.cacheHitRate * 10)) / 10)
    setLastRefresh(nowTime())
    setTick(t => t + 1)
    // Occasionally push a new fake recent query
    if (Math.random() > 0.4) {
      const q = LIVE_QUERIES[Math.floor(Math.random() * LIVE_QUERIES.length)]
      const entry: QueryLog = {
        ...q,
        duration: jitter(q.duration),
        timestamp: nowTime(),
      }
      setRecentLog(prev => [entry, ...prev].slice(0, 10))
    }
  }, [])

  useEffect(() => {
    if (!live || interval === 'off') {
      if (timerRef.current) clearInterval(timerRef.current)
      return
    }
    timerRef.current = setInterval(refresh, interval * 1000)
    return () => { if (timerRef.current) clearInterval(timerRef.current) }
  }, [live, interval, refresh])

  // Display values: live-jittered when active, static otherwise
  const displayQps = live ? qps : METRICS.queriesPerSecond
  const displayConns = live ? connections : METRICS.activeConnections
  const displayCache = live ? cacheHit : METRICS.cacheHitRate

  return (
    <div className="flex-1 overflow-y-auto">
      {/* Header */}
      <div className="border-b border-border px-6 py-4 flex items-center justify-between">
        <div>
          <h1 className="text-sm font-semibold text-text-primary">Dashboard</h1>
          <p className="text-xs text-text-secondary mt-0.5">
            AxiomDB v0.1 · localhost:3306
            {live && <span className="ml-2 text-accent font-mono">· refreshed {lastRefresh}</span>}
          </p>
        </div>
        <div className="flex items-center gap-3">
          {/* Live toggle + interval */}
          <div className="flex items-center gap-1.5 bg-elevated border border-border rounded-lg p-1">
            <button
              onClick={() => setLive(p => !p)}
              className={[
                'flex items-center gap-1.5 px-2.5 py-1 rounded text-xs font-semibold transition-all',
                live
                  ? 'bg-accent text-white shadow-sm'
                  : 'text-text-secondary hover:text-text-primary',
              ].join(' ')}>
              <span className={['w-1.5 h-1.5 rounded-full', live ? 'bg-white animate-pulse' : 'bg-text-secondary'].join(' ')} />
              Live
            </button>
            {INTERVAL_OPTIONS.map(opt => (
              <button
                key={String(opt.value)}
                onClick={() => setInterval_(opt.value)}
                className={[
                  'px-2 py-1 rounded text-xs font-medium transition-colors',
                  interval === opt.value
                    ? 'bg-surface text-text-primary shadow-sm'
                    : 'text-text-secondary hover:text-text-primary',
                ].join(' ')}>
                {opt.label}
              </button>
            ))}
          </div>

          <div className="flex items-center gap-2 text-xs text-accent">
            <div className="w-1.5 h-1.5 rounded-full bg-accent animate-pulse" />
            Connected
          </div>
        </div>
      </div>

      <div className="p-6 space-y-6">
        {/* Metrics grid */}
        <div className="grid grid-cols-4 gap-3">
          <MetricCard icon={Zap} label="Queries / sec"
            value={formatNumber(displayQps)}
            sub="avg 8.3ms" trend="↑ 12% from yesterday" />
          <MetricCard icon={Wifi} label="Connections"
            value={`${displayConns} / ${METRICS.maxConnections}`}
            sub={`${Math.round((displayConns / METRICS.maxConnections) * 100)}% utilized`} />
          <MetricCard icon={Database} label="Database size"
            value={METRICS.dbSize}
            sub={`WAL: ${METRICS.walSize}`} />
          <MetricCard icon={TrendingUp} label="Cache hit rate"
            value={`${displayCache}%`}
            sub="Buffer pool" trend="↑ 2.1% this hour" />
        </div>

        {/* Second row */}
        <div className="grid grid-cols-3 gap-3">
          <MetricCard icon={Clock} label="Uptime" value={METRICS.uptime} />
          <div className="bg-surface border border-border rounded-lg p-4">
            <div className="text-xs text-text-secondary mb-2">Query throughput</div>
            <Sparkline data={SPARKLINE_DATA.qps} />
          </div>
          <div className="bg-surface border border-border rounded-lg p-4">
            <div className="text-xs text-text-secondary mb-2">Active connections</div>
            <Sparkline data={SPARKLINE_DATA.connections} />
          </div>
        </div>

        {/* Recent queries */}
        <div className="bg-surface border border-border rounded-lg">
          <div className="px-4 py-3 border-b border-border flex items-center justify-between">
            <span className="text-xs font-semibold text-text-primary">Recent queries</span>
            <span className="text-xs text-text-secondary">last {recentLog.length}</span>
          </div>
          <table className="w-full">
            <thead>
              <tr className="border-b border-border">
                {['Query', 'Duration', 'Rows', 'Time', 'Status'].map(h => (
                  <th key={h} className="text-left px-4 py-2 text-[10px] font-semibold tracking-wider text-text-secondary uppercase">
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {recentLog.map((q, i) => (
                <tr key={i} className="border-b border-border/50 hover:bg-elevated transition-colors">
                  <td className="px-4 py-2.5 font-mono text-xs text-text-secondary max-w-xs truncate">
                    {q.query}
                  </td>
                  <td className="px-4 py-2.5 text-xs font-mono text-text-primary">
                    {q.duration}ms
                  </td>
                  <td className="px-4 py-2.5 text-xs font-mono text-text-secondary">
                    {q.rows.toLocaleString()}
                  </td>
                  <td className="px-4 py-2.5 text-xs font-mono text-text-secondary">
                    {q.timestamp}
                  </td>
                  <td className="px-4 py-2.5">
                    <span className={`text-[10px] px-1.5 py-0.5 rounded font-semibold ${
                      q.status === 'ok'
                        ? 'bg-accent/10 text-accent'
                        : 'bg-error/10 text-error'
                    }`}>
                      {q.status}
                    </span>
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>

        {/* Slow queries */}
        <div className="bg-surface border border-border rounded-lg">
          <div className="px-4 py-3 border-b border-border flex items-center gap-2">
            <AlertTriangle className="w-3.5 h-3.5 text-warning" />
            <span className="text-xs font-semibold text-text-primary">Slow Queries</span>
            <span className="text-[10px] px-1.5 py-0.5 rounded bg-warning/10 text-warning font-semibold">&gt;100ms</span>
          </div>
          <table className="w-full">
            <thead>
              <tr className="border-b border-border">
                {['Query', 'Table', 'Duration', 'Time'].map(h => (
                  <th key={h} className="text-left px-4 py-2 text-[10px] font-semibold tracking-wider text-text-secondary uppercase">
                    {h}
                  </th>
                ))}
              </tr>
            </thead>
            <tbody>
              {SLOW_QUERIES.map((q, i) => (
                <tr key={i} className="border-b border-border/50 hover:bg-elevated transition-colors">
                  <td className="px-4 py-2.5 font-mono text-xs text-text-secondary max-w-xs truncate">
                    {q.query}
                  </td>
                  <td className="px-4 py-2.5">
                    <span className="font-mono text-xs text-blue-400">{q.table}</span>
                  </td>
                  <td className="px-4 py-2.5">
                    <span className={[
                      'text-xs font-mono font-semibold',
                      q.duration >= 300 ? 'text-error' : 'text-warning',
                    ].join(' ')}>
                      {q.duration}ms
                    </span>
                  </td>
                  <td className="px-4 py-2.5 text-xs font-mono text-text-secondary">
                    {q.timestamp}
                  </td>
                </tr>
              ))}
            </tbody>
          </table>
        </div>
      </div>
    </div>
  )
}
